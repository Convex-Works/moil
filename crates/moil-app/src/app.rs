//! The event loop: the tray, the windows and the agent, wired together.
//!
//! Decisions live elsewhere: [`TrayView`] decides what the tray shows, and
//! each window's module decides what its page shows and what its buttons
//! do. This module carries events to them and carries out what they ask
//! for, on the main thread for windows and on Tokio for the agent.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use moil_agent::{Review, ServiceId, Snapshot};
use moil_proto::BundleHash;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Theme, WindowBuilder, WindowId};
use tokio::task::AbortHandle;
use tray_icon::menu::{Menu, MenuEvent, MenuId};
use url::Url;
use wry::{NewWindowResponse, WebView, WebViewBuilder};

use crate::backend::{Backend, Pairing, PairingCode};
use crate::ipc::{self, ConfirmMessage, PairMessage, ReviewMessage};
use crate::links::{DeepLink, Navigation, browser_url};
use crate::login::LoginItem;
use crate::page;
use crate::pair::{PairEffect, PairWindow};
use crate::platform;
use crate::review::{ReviewEffect, ReviewWindow};
use crate::tray::Tray;
use crate::unpair::{UnpairEffect, UnpairWindow};
use crate::view::{Action, TrayView, ViewContext};

/// How long quitting waits for the agent to report the running job and
/// disconnect: the runtime gives a stopped script up to 10 seconds, and the
/// agent waits up to 5 more for services to acknowledge.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

/// Everything that wakes the event loop, besides the platform's own events.
#[derive(Debug)]
pub enum AppEvent {
    Snapshot(Box<Snapshot>),
    Menu(MenuId),
    /// A page posted a message.
    Ipc {
        window: WindowId,
        body: String,
    },
    /// A `moil://` link, from the command line.
    Link(String),
    Reviewed {
        window: WindowId,
        result: Result<Box<Review>, String>,
    },
    PairStarted {
        window: WindowId,
        attempt: u64,
        result: Result<PairingCode, String>,
    },
    Paired {
        window: WindowId,
        attempt: u64,
        result: Result<String, String>,
    },
    /// Approving, revoking or unpairing finished.
    Done {
        window: WindowId,
        result: Result<(), String>,
    },
    /// The agent has shut down; the app can exit.
    ShutDown,
}

/// Runs the app until the owner quits. `backend` is the agent, or why it
/// couldn't start; `links` are `moil://` links from the command line;
/// `log_file` is where the app logs, if it keeps a file.
pub fn run(
    runtime: tokio::runtime::Runtime,
    backend: Result<Arc<dyn Backend>, String>,
    login: Box<dyn LoginItem>,
    links: Vec<String>,
    log_file: Option<PathBuf>,
) -> ! {
    let mut event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    platform::configure(&mut event_loop);
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = menu_proxy.send_event(AppEvent::Menu(event.id));
    }));
    for link in links {
        let _ = proxy.send_event(AppEvent::Link(link));
    }

    let mut app = App {
        backend,
        runtime,
        proxy,
        login,
        log_file,
        tray: None,
        snapshot: Snapshot::default(),
        deferred: HashSet::new(),
        windows: HashMap::new(),
        quitting: false,
        exit: false,
        refresh_at: None,
        _edit_shortcuts: None,
    };
    event_loop.run(move |event, target, control_flow| {
        match event {
            Event::NewEvents(StartCause::Init) => app.start(target),
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => app.refresh_tray(),
            Event::UserEvent(event) => app.handle(event, target),
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => app.close(window_id),
            Event::Opened { urls } => {
                for url in urls {
                    app.open_link(url.as_str(), target);
                }
            }
            _ => {}
        }
        *control_flow = match (app.exit, app.refresh_at) {
            (true, _) => ControlFlow::Exit,
            (false, Some(at)) => ControlFlow::WaitUntil(at),
            (false, None) => ControlFlow::Wait,
        };
    })
}

struct App {
    backend: Result<Arc<dyn Backend>, String>,
    runtime: tokio::runtime::Runtime,
    proxy: EventLoopProxy<AppEvent>,
    login: Box<dyn LoginItem>,
    log_file: Option<PathBuf>,
    tray: Option<Tray>,
    snapshot: Snapshot,
    /// Reviews the owner closed without approving.
    deferred: HashSet<(ServiceId, BundleHash)>,
    windows: HashMap<WindowId, Window>,
    quitting: bool,
    exit: bool,
    refresh_at: Option<Instant>,
    _edit_shortcuts: Option<Menu>,
}

/// A window showing one of the app's pages.
struct Window {
    // Dropped before the window it lives in.
    webview: WebView,
    window: tao::window::Window,
    content: Content,
}

enum Content {
    Review(ReviewWindow),
    Pair {
        flow: PairWindow,
        task: Option<AbortHandle>,
    },
    Unpair(UnpairWindow),
    /// A message with one button that closes it; `quit` when closing it
    /// quits the app.
    Notice {
        quit: bool,
    },
}

/// How a window opens.
struct Frame {
    title: String,
    size: (f64, f64),
    resizable: bool,
}

impl Frame {
    fn review(title: String) -> Self {
        Self {
            title,
            size: (900.0, 780.0),
            resizable: true,
        }
    }

    fn dialog(title: String, height: f64) -> Self {
        Self {
            title,
            size: (460.0, height),
            resizable: false,
        }
    }
}

impl App {
    fn start(&mut self, target: &EventLoopWindowTarget<AppEvent>) {
        self._edit_shortcuts = platform::edit_shortcuts();
        let backend = match &self.backend {
            Ok(backend) => backend.clone(),
            Err(error) => {
                let html = page::notice("moil couldn't start", error, "Quit");
                let frame = Frame::dialog("moil".into(), 260.0);
                self.open(target, frame, &html, Content::Notice { quit: true });
                return;
            }
        };
        let mut snapshots = backend.subscribe();
        self.snapshot = snapshots.borrow_and_update().clone();
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            while snapshots.changed().await.is_ok() {
                let snapshot = Box::new(snapshots.borrow_and_update().clone());
                if proxy.send_event(AppEvent::Snapshot(snapshot)).is_err() {
                    break;
                }
            }
        });
        match Tray::new(self.view()) {
            Ok(tray) => self.tray = Some(tray),
            Err(e) => tracing::error!("{e}"),
        }
        tracing::info!("moil is running in the menu bar");
    }

    fn view(&mut self) -> TrayView {
        let view = TrayView::new(
            &self.snapshot,
            &ViewContext {
                now: Instant::now(),
                clock: SystemTime::now(),
                open_at_login: self.login.enabled(),
                has_log: self.log_file.is_some(),
                quitting: self.quitting,
                deferred: &self.deferred,
            },
        );
        self.refresh_at = view.refresh_at;
        view
    }

    fn refresh_tray(&mut self) {
        let view = self.view();
        if let Some(tray) = &mut self.tray {
            tray.show(view);
        }
    }

    fn handle(&mut self, event: AppEvent, target: &EventLoopWindowTarget<AppEvent>) {
        match event {
            AppEvent::Snapshot(snapshot) => {
                self.snapshot = *snapshot;
                self.refresh_tray();
            }
            AppEvent::Menu(id) => {
                let action = self.tray.as_ref().and_then(|t| t.action(&id)).cloned();
                if let Some(action) = action {
                    self.act(action, target);
                }
            }
            AppEvent::Ipc { window, body } => self.message(window, &body),
            AppEvent::Link(link) => self.open_link(&link, target),
            AppEvent::Reviewed { window, result } => {
                if let Some(Content::Review(flow)) = self.content(window) {
                    flow.loaded(result.map(|review| *review));
                    self.render(window);
                }
            }
            AppEvent::PairStarted {
                window,
                attempt,
                result,
            } => {
                if let Some(Content::Pair { flow, .. }) = self.content(window) {
                    let effect = flow.started(attempt, result);
                    self.render(window);
                    self.pair_effect(window, effect);
                }
            }
            AppEvent::Paired {
                window,
                attempt,
                result,
            } => {
                if let Some(Content::Pair { flow, task }) = self.content(window) {
                    flow.finished(attempt, result);
                    *task = None;
                    self.render(window);
                }
            }
            AppEvent::Done { window, result } => match self.content(window) {
                Some(Content::Review(flow)) => {
                    let effect = flow.finished(result);
                    self.render(window);
                    self.review_effect(window, effect);
                }
                Some(Content::Unpair(flow)) => {
                    let effect = flow.finished(result);
                    self.render(window);
                    self.unpair_effect(window, effect);
                }
                _ => {}
            },
            AppEvent::ShutDown => self.exit = true,
        }
    }

    fn act(&mut self, action: Action, target: &EventLoopWindowTarget<AppEvent>) {
        let Ok(backend) = self.backend.clone() else {
            return;
        };
        match action {
            Action::StopJob => backend.stop_job(),
            Action::SetPaused(paused) => backend.set_paused(paused),
            Action::Pair => self.pair(None, target),
            Action::PairAgain(url) => self.pair(Some(&url), target),
            Action::Review { service, hash } => self.review(service, hash, target),
            Action::Unpair(service) => self.confirm_unpair(&service, target),
            Action::ToggleOpenAtLogin => {
                let enabled = self.login.enabled() == Some(true);
                if let Err(e) = self.login.set_enabled(!enabled) {
                    let html = page::notice("Couldn't change Open at Login", &e, "OK");
                    let frame = Frame::dialog("moil".into(), 240.0);
                    self.open(target, frame, &html, Content::Notice { quit: false });
                }
                self.refresh_tray();
            }
            Action::ShowLog => {
                if let Some(path) = &self.log_file {
                    platform::reveal(path);
                }
            }
            Action::Quit => self.quit(),
        }
    }

    fn pair(&mut self, url: Option<&Url>, target: &EventLoopWindowTarget<AppEvent>) {
        if self.focus(|c| matches!(c, Content::Pair { .. })) {
            return;
        }
        let flow = PairWindow::new(url);
        let html = flow.render(Instant::now());
        let frame = Frame::dialog(flow.title().into(), 370.0);
        self.open(target, frame, &html, Content::Pair { flow, task: None });
    }

    fn review(
        &mut self,
        service: ServiceId,
        hash: BundleHash,
        target: &EventLoopWindowTarget<AppEvent>,
    ) {
        if self.focus(|c| matches!(c, Content::Review(r) if r.subject() == (&service, hash))) {
            return;
        }
        let name = self
            .snapshot
            .service(&service)
            .map_or_else(|| service.to_string(), |s| s.name.clone());
        let flow = ReviewWindow::new(service.clone(), name, hash);
        let html = flow.render(SystemTime::now());
        let frame = Frame::review(flow.title());
        let Some(window) = self.open(target, frame, &html, Content::Review(flow)) else {
            return;
        };
        self.spawn(move |backend| async move {
            let result = backend
                .review(service, hash)
                .await
                .map(Box::new)
                .map_err(|e| e.to_string());
            AppEvent::Reviewed { window, result }
        });
    }

    fn confirm_unpair(&mut self, service: &ServiceId, target: &EventLoopWindowTarget<AppEvent>) {
        if self.focus(|c| matches!(c, Content::Unpair(u) if u.service() == service)) {
            return;
        }
        let Some(status) = self.snapshot.service(service) else {
            return;
        };
        let flow = UnpairWindow::new(status);
        let html = flow.render();
        let frame = Frame::dialog(flow.title(), 250.0);
        self.open(target, frame, &html, Content::Unpair(flow));
    }

    /// Handles a `moil://` link: it can only ever open the pair window,
    /// which asks the owner before doing anything.
    fn open_link(&mut self, link: &str, target: &EventLoopWindowTarget<AppEvent>) {
        if self.backend.is_err() || self.quitting {
            return;
        }
        let url = match DeepLink::parse(link) {
            Ok(DeepLink::Pair { url }) => url,
            Err(e) => {
                tracing::warn!("{e}: {link}");
                let html = page::notice("moil can't open this link", &e.to_string(), "OK");
                let frame = Frame::dialog("moil".into(), 240.0);
                self.open(target, frame, &html, Content::Notice { quit: false });
                return;
            }
        };
        let existing = self
            .windows
            .iter_mut()
            .find_map(|(id, w)| match &mut w.content {
                Content::Pair { flow, .. } => Some((*id, flow)),
                _ => None,
            });
        match existing {
            Some((id, flow)) => {
                flow.link(&url);
                self.render(id);
                self.focus(|c| matches!(c, Content::Pair { .. }));
            }
            None => {
                let flow = PairWindow::from_link(&url);
                let html = flow.render(Instant::now());
                let frame = Frame::dialog(flow.title().into(), 370.0);
                self.open(target, frame, &html, Content::Pair { flow, task: None });
            }
        }
    }

    /// A page posted `body`: each page understands only its own messages.
    fn message(&mut self, window: WindowId, body: &str) {
        let Some(content) = self.content(window) else {
            return;
        };
        match content {
            Content::Review(flow) => match ipc::parse::<ReviewMessage>(body) {
                Ok(message) => {
                    let effect = flow.on_message(message);
                    self.render(window);
                    self.review_effect(window, effect);
                }
                Err(e) => tracing::warn!("{e}"),
            },
            Content::Pair { flow, .. } => match ipc::parse::<PairMessage>(body) {
                Ok(message) => {
                    let effect = flow.on_message(message);
                    self.render(window);
                    self.pair_effect(window, effect);
                }
                Err(e) => tracing::warn!("{e}"),
            },
            Content::Unpair(flow) => match ipc::parse::<ConfirmMessage>(body) {
                Ok(message) => {
                    let effect = flow.on_message(message);
                    self.render(window);
                    self.unpair_effect(window, effect);
                }
                Err(e) => tracing::warn!("{e}"),
            },
            Content::Notice { .. } => match ipc::parse::<ConfirmMessage>(body) {
                Ok(_) => self.close(window),
                Err(e) => tracing::warn!("{e}"),
            },
        }
    }

    fn review_effect(&mut self, window: WindowId, effect: Option<ReviewEffect>) {
        match effect {
            None => {}
            Some(ReviewEffect::Close) => self.close(window),
            Some(ReviewEffect::Approve(review)) => self.spawn(move |backend| async move {
                let result = backend.approve(*review).await.map_err(|e| e.to_string());
                AppEvent::Done { window, result }
            }),
            Some(ReviewEffect::Revoke { service, hash }) => self.spawn(move |backend| async move {
                let result = backend
                    .revoke(service, hash)
                    .await
                    .map_err(|e| e.to_string());
                AppEvent::Done { window, result }
            }),
        }
    }

    fn pair_effect(&mut self, window: WindowId, effect: Option<PairEffect>) {
        match effect {
            None => {}
            Some(PairEffect::Close) => self.close(window),
            Some(PairEffect::OpenBrowser(url)) => platform::open_in_browser(&url),
            Some(PairEffect::Start { url, attempt }) => {
                let Ok(backend) = self.backend.clone() else {
                    return;
                };
                let proxy = self.proxy.clone();
                let handle = self.runtime.spawn(async move {
                    let result = backend.pair(url.to_string()).await;
                    let confirmed = match result {
                        Ok(Pairing { code, confirmed }) => {
                            let started = Ok(code);
                            let _ = proxy.send_event(AppEvent::PairStarted {
                                window,
                                attempt,
                                result: started,
                            });
                            confirmed
                        }
                        Err(e) => {
                            let _ = proxy.send_event(AppEvent::PairStarted {
                                window,
                                attempt,
                                result: Err(e.to_string()),
                            });
                            return;
                        }
                    };
                    let result = confirmed
                        .await
                        .map(|service| service.name)
                        .map_err(|e| e.to_string());
                    let _ = proxy.send_event(AppEvent::Paired {
                        window,
                        attempt,
                        result,
                    });
                });
                if let Some(Content::Pair { task, .. }) = self.content(window)
                    && let Some(previous) = task.replace(handle.abort_handle())
                {
                    previous.abort();
                }
            }
        }
    }

    fn unpair_effect(&mut self, window: WindowId, effect: Option<UnpairEffect>) {
        match effect {
            None => {}
            Some(UnpairEffect::Close) => self.close(window),
            Some(UnpairEffect::Unpair(service)) => self.spawn(move |backend| async move {
                let result = backend.unpair(service).await.map_err(|e| e.to_string());
                AppEvent::Done { window, result }
            }),
        }
    }

    /// Runs `work` against the agent on Tokio and delivers the event it
    /// ends with.
    fn spawn<F, Fut>(&self, work: F)
    where
        F: FnOnce(Arc<dyn Backend>) -> Fut,
        Fut: Future<Output = AppEvent> + Send + 'static,
    {
        let Ok(backend) = self.backend.clone() else {
            return;
        };
        let proxy = self.proxy.clone();
        let work = work(backend);
        self.runtime.spawn(async move {
            let _ = proxy.send_event(work.await);
        });
    }

    fn content(&mut self, window: WindowId) -> Option<&mut Content> {
        self.windows.get_mut(&window).map(|w| &mut w.content)
    }

    /// Shows a window's page again, if its state has moved on from it.
    fn render(&mut self, id: WindowId) {
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        let (title, html) = match &mut window.content {
            Content::Review(flow) => {
                if !flow.needs_render() {
                    return;
                }
                (flow.title(), flow.render(SystemTime::now()))
            }
            Content::Pair { flow, .. } => {
                if !flow.needs_render() {
                    return;
                }
                (flow.title().into(), flow.render(Instant::now()))
            }
            Content::Unpair(flow) => {
                if !flow.needs_render() {
                    return;
                }
                (flow.title(), flow.render())
            }
            Content::Notice { .. } => return,
        };
        window.window.set_title(&title);
        if let Err(e) = window.webview.load_html(&html) {
            tracing::error!("couldn't show a page: {e}");
        }
    }

    /// Brings the first window whose content matches to the front.
    fn focus(&self, matches: impl Fn(&Content) -> bool) -> bool {
        let found = self.windows.values().find(|w| matches(&w.content));
        if let Some(window) = found {
            window.window.set_focus();
        }
        found.is_some()
    }

    fn open(
        &mut self,
        target: &EventLoopWindowTarget<AppEvent>,
        frame: Frame,
        html: &str,
        content: Content,
    ) -> Option<WindowId> {
        let window = WindowBuilder::new()
            .with_title(&frame.title)
            .with_inner_size(LogicalSize::new(frame.size.0, frame.size.1))
            .with_min_inner_size(LogicalSize::new(420.0, 220.0))
            .with_resizable(frame.resizable)
            .build(target)
            .map_err(|e| tracing::error!("couldn't open a window: {e}"))
            .ok()?;
        let id = window.id();
        let background = match window.theme() {
            Theme::Dark => (30, 30, 30, 255),
            _ => (255, 255, 255, 255),
        };
        let proxy = self.proxy.clone();
        let webview = WebViewBuilder::new()
            .with_html(html)
            .with_background_color(background)
            .with_ipc_handler(move |request| {
                let body = request.into_body();
                let _ = proxy.send_event(AppEvent::Ipc { window: id, body });
            })
            .with_navigation_handler(|url| match Navigation::of(&url) {
                Navigation::Stay => true,
                Navigation::Browser(url) => {
                    platform::open_in_browser(&url);
                    false
                }
                Navigation::Block => {
                    tracing::warn!("blocked a page from navigating to {url}");
                    false
                }
            })
            .with_new_window_req_handler(|url, _| {
                if let Some(url) = browser_url(&url) {
                    platform::open_in_browser(&url);
                }
                NewWindowResponse::Deny
            })
            .with_download_started_handler(|_, _| false)
            .build(&window)
            .map_err(|e| tracing::error!("couldn't show a page: {e}"))
            .ok()?;
        window.set_focus();
        tracing::debug!(title = frame.title, "opened a window");
        self.windows.insert(
            id,
            Window {
                webview,
                window,
                content,
            },
        );
        Some(id)
    }

    /// Closes a window. Closing a review puts it off; closing the pair
    /// window abandons the pairing.
    fn close(&mut self, id: WindowId) {
        let Some(window) = self.windows.remove(&id) else {
            return;
        };
        match window.content {
            Content::Review(flow) if !flow.approved() => {
                let (service, hash) = flow.subject();
                let pending = self
                    .snapshot
                    .pending_reviews()
                    .any(|(s, b)| s.id == *service && b.info.hash == hash);
                if pending {
                    self.deferred.insert((service.clone(), hash));
                    self.refresh_tray();
                }
            }
            Content::Pair { task, .. } => {
                if let Some(task) = task {
                    task.abort();
                }
            }
            Content::Review(_) | Content::Unpair(_) => {}
            Content::Notice { quit } => {
                if quit {
                    self.quit();
                }
            }
        }
    }

    /// Shuts the agent down, then exits. The running job is reported as
    /// interrupted, so its service can give it to another machine.
    fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        let ids: Vec<_> = self.windows.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
        self.refresh_tray();
        let Ok(backend) = self.backend.clone() else {
            self.exit = true;
            return;
        };
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, backend.shutdown())
                .await
                .is_err()
            {
                tracing::warn!("the agent took too long to shut down");
            }
            let _ = proxy.send_event(AppEvent::ShutDown);
        });
    }
}
