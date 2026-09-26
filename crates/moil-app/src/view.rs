//! What the tray shows, as plain data: [`TrayView::new`] turns the agent's
//! [`Snapshot`] into the menu, the icon's state and the title beside it.
//! The `tray` module makes it real; everything here is pure.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use moil_agent::{Connection, JobStatus, ServiceId, ServiceStatus, Snapshot};
use moil_proto::{BundleHash, Phase};
use url::Url;

use crate::format;

/// Everything the tray shows.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayView {
    pub icon: IconState,
    /// Beside the icon: the running job's progress.
    pub title: Option<String>,
    pub tooltip: String,
    pub menu: MenuModel,
    /// When the view goes stale without a new snapshot, such as a
    /// reconnection countdown ticking.
    pub refresh_at: Option<Instant>,
}

/// What the view depends on besides the snapshot.
#[derive(Debug, Clone)]
pub struct ViewContext<'a> {
    pub now: Instant,
    /// Whether moil opens at login; `None` where that can't be changed.
    pub open_at_login: Option<bool>,
    /// The app is shutting the agent down.
    pub quitting: bool,
    /// Reviews the owner put off with "Not now". They stay in the menu but
    /// no longer ask for attention.
    pub deferred: &'a HashSet<(ServiceId, BundleHash)>,
}

/// The icon: what the machine is doing, and whether the owner is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IconState {
    pub activity: Activity,
    /// A bundle waits for review, or a service needs pairing again.
    pub attention: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    Idle,
    /// A job runs. It carries on while paused, so this wins over paused.
    Running,
    Paused,
}

/// The tray menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuModel {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Text that can't be clicked, such as the status line.
    Text(Label),
    /// Something to do.
    Item {
        label: Label,
        action: Action,
    },
    /// A setting that is on or off. Disabled where it can't be changed.
    Check {
        label: Label,
        checked: bool,
        enabled: bool,
        action: Action,
    },
    Submenu {
        label: Label,
        entries: Vec<Entry>,
    },
    Separator,
}

/// A menu label: `text`, then an optional de-emphasized `detail`. Either
/// may be empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub text: String,
    pub detail: Option<String>,
}

/// What a menu item does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    StopJob,
    SetPaused(bool),
    Pair,
    /// Pair with a service that removed this machine, at its address.
    PairAgain(Url),
    Review {
        service: ServiceId,
        hash: BundleHash,
    },
    Unpair(ServiceId),
    ToggleOpenAtLogin,
    Quit,
}

impl Label {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            detail: None,
        }
    }

    /// A label that is all detail, for secondary information.
    pub fn quiet(detail: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            detail: Some(detail.into()),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl std::fmt::Display for Label {
    /// The label as one line of text: `text — detail`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&*self.text, &self.detail) {
            (text, None) => f.write_str(text),
            ("", Some(detail)) => f.write_str(detail),
            (text, Some(detail)) => write!(f, "{text} — {detail}"),
        }
    }
}

impl MenuModel {
    /// Whether `other` has the same items doing the same things, so that
    /// updating labels in place turns this menu into `other`. Progress
    /// ticks keep the shape; a new review or service changes it.
    pub fn same_shape(&self, other: &MenuModel) -> bool {
        fn same(a: &[Entry], b: &[Entry]) -> bool {
            a.len() == b.len()
                && a.iter().zip(b).all(|pair| match pair {
                    (Entry::Text(_), Entry::Text(_)) | (Entry::Separator, Entry::Separator) => true,
                    (Entry::Item { action: a, .. }, Entry::Item { action: b, .. })
                    | (Entry::Check { action: a, .. }, Entry::Check { action: b, .. }) => a == b,
                    (Entry::Submenu { entries: a, .. }, Entry::Submenu { entries: b, .. }) => {
                        same(a, b)
                    }
                    _ => false,
                })
        }
        same(&self.entries, &other.entries)
    }

    /// Every entry, submenus' entries right after the submenu, in order.
    pub fn flatten(&self) -> Vec<&Entry> {
        fn walk<'a>(entries: &'a [Entry], out: &mut Vec<&'a Entry>) {
            for entry in entries {
                out.push(entry);
                if let Entry::Submenu { entries, .. } = entry {
                    walk(entries, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.entries, &mut out);
        out
    }

    /// The action of the item labeled `text`.
    #[cfg(test)]
    pub fn action(&self, text: &str) -> Option<&Action> {
        self.flatten().into_iter().find_map(|entry| match entry {
            Entry::Item { label, action } | Entry::Check { label, action, .. }
                if label.to_string() == text =>
            {
                Some(action)
            }
            _ => None,
        })
    }
}

impl TrayView {
    pub fn new(snapshot: &Snapshot, cx: &ViewContext<'_>) -> Self {
        let status = status(snapshot);
        Self {
            icon: IconState::new(snapshot, cx.deferred),
            title: snapshot
                .job
                .as_ref()
                .and_then(progress)
                .map(|percent| format!("{percent}%")),
            tooltip: format!("moil: {status}"),
            menu: menu(snapshot, cx, status),
            refresh_at: refresh_at(snapshot, cx.now),
        }
    }
}

impl IconState {
    pub fn new(snapshot: &Snapshot, deferred: &HashSet<(ServiceId, BundleHash)>) -> Self {
        let activity = match (&snapshot.job, snapshot.paused) {
            (Some(_), _) => Activity::Running,
            (None, true) => Activity::Paused,
            (None, false) => Activity::Idle,
        };
        let review_waiting = snapshot
            .pending_reviews()
            .any(|(service, bundle)| !deferred.contains(&(service.id.clone(), bundle.info.hash)));
        let service_lost = snapshot
            .services
            .iter()
            .any(|s| matches!(s.connection, Connection::Unpaired | Connection::Replaced));
        Self {
            activity,
            attention: review_waiting || service_lost,
        }
    }
}

/// The running job's progress in whole percent, once the script reports
/// any.
fn progress(job: &JobStatus) -> Option<u8> {
    match job.phase {
        Phase::Running => job.fraction.and_then(format::percent),
        _ => None,
    }
}

/// The first line of the menu.
fn status(snapshot: &Snapshot) -> Label {
    match &snapshot.job {
        Some(job) => {
            let what = job.title.as_deref().unwrap_or(&job.bundle_name);
            let service = snapshot
                .services
                .iter()
                .find(|s| s.id == job.service)
                .map_or(job.service.as_str(), |s| &s.name);
            let label = Label::new(format!(
                "Running “{}” for {}",
                clean(what, 48),
                clean(service, 32)
            ));
            match (job.phase, progress(job)) {
                (_, Some(percent)) => label.with_detail(format!("{percent}%")),
                (Phase::Preparing, _) => label.with_detail("preparing"),
                (Phase::Downloading, _) => label.with_detail("downloading inputs"),
                (Phase::Uploading, _) => label.with_detail("uploading results"),
                (Phase::Running, None) => label,
            }
        }
        None if snapshot.paused => Label::new("Paused").with_detail("taking no new jobs"),
        None if snapshot.services.is_empty() => Label::new("Idle"),
        None => Label::new("Idle").with_detail("waiting for jobs"),
    }
}

fn menu(snapshot: &Snapshot, cx: &ViewContext<'_>, status: Label) -> MenuModel {
    use Entry::*;
    if cx.quitting {
        let what = if snapshot.job.is_some() {
            "Stopping the job and quitting…"
        } else {
            "Quitting…"
        };
        return MenuModel {
            entries: vec![Text(Label::new(what))],
        };
    }

    let mut entries = vec![Text(status)];
    if let Some(job) = &snapshot.job {
        let mut detail = clean(&job.bundle_name, 32);
        if let Some(message) = &job.message {
            detail = format!("{detail}: {}", clean(message, 48));
        }
        entries.push(Text(Label::quiet(detail)));
        if snapshot.paused {
            entries.push(Text(Label::quiet("Paused: no new jobs after this one")));
        }
        entries.push(Item {
            label: Label::new("Stop Current Job"),
            action: Action::StopJob,
        });
    }

    entries.push(Separator);
    if snapshot.services.is_empty() {
        entries.push(Text(Label::quiet("Not paired with any service")));
    }
    for (i, service) in snapshot.services.iter().enumerate() {
        if i > 0 {
            entries.push(Separator);
        }
        entries.push(Submenu {
            label: Label::new(clean(&service.name, 32)).with_detail(host(&service.base_url)),
            entries: service_menu(service, cx.now),
        });
        entries.push(Text(Label::quiet(connection(&service.connection, cx.now))));
        for bundle in service.bundles.iter().filter(|b| !b.approved) {
            entries.push(Item {
                label: Label::new(format!(
                    "Review {} {}…",
                    clean(&bundle.info.name, 32),
                    clean(&bundle.info.version, 16)
                )),
                action: Action::Review {
                    service: service.id.clone(),
                    hash: bundle.info.hash,
                },
            });
        }
        if service.connection == Connection::Unpaired {
            entries.push(Item {
                label: Label::new(format!("Pair with {} Again…", clean(&service.name, 32))),
                action: Action::PairAgain(service.base_url.clone()),
            });
        }
    }

    entries.push(Separator);
    entries.push(Item {
        label: Label::new(if snapshot.paused { "Resume" } else { "Pause" }),
        action: Action::SetPaused(!snapshot.paused),
    });
    entries.push(Item {
        label: Label::new("Pair with a Service…"),
        action: Action::Pair,
    });
    entries.push(Separator);
    entries.push(Check {
        label: Label::new("Open at Login"),
        checked: cx.open_at_login == Some(true),
        enabled: cx.open_at_login.is_some(),
        action: Action::ToggleOpenAtLogin,
    });
    entries.push(Item {
        label: Label::new("Quit moil"),
        action: Action::Quit,
    });
    MenuModel { entries }
}

/// A service's submenu: where it is, its bundles, and unpairing.
fn service_menu(service: &ServiceStatus, now: Instant) -> Vec<Entry> {
    use Entry::*;
    let mut entries = vec![
        Text(Label::quiet(clean(service.base_url.as_str(), 64))),
        Text(Label::new(connection(&service.connection, now))),
    ];
    if service.insecure {
        entries.push(Text(Label::quiet("Unencrypted connection (http)")));
    }
    entries.push(Separator);
    if service.bundles.is_empty() {
        entries.push(Text(Label::quiet("Offers no job bundles")));
    }
    for bundle in &service.bundles {
        let name = format!(
            "{} {}",
            clean(&bundle.info.name, 32),
            clean(&bundle.info.version, 16)
        );
        entries.push(Item {
            label: if bundle.approved {
                Label::new(name).with_detail("approved")
            } else {
                Label::new(format!("Review {name}…"))
            },
            action: Action::Review {
                service: service.id.clone(),
                hash: bundle.info.hash,
            },
        });
    }
    entries.push(Separator);
    entries.push(Item {
        label: Label::new(format!("Unpair {}…", clean(&service.name, 32))),
        action: Action::Unpair(service.id.clone()),
    });
    entries
}

fn connection(connection: &Connection, now: Instant) -> String {
    match connection {
        Connection::Connecting => "Connecting…".into(),
        Connection::Connected => "Connected".into(),
        Connection::Disconnected { error, retry_at } => {
            let error = clean(error, 48);
            match retry_at.checked_duration_since(now) {
                Some(wait) if !wait.is_zero() => {
                    format!("Offline: {error}. Retrying in {}", format::wait(wait))
                }
                _ => format!("Offline: {error}. Retrying now"),
            }
        }
        Connection::Unpaired => "Removed this Mac; pair again to use it".into(),
        Connection::Replaced => "Taken over by another connection using this Mac's token".into(),
    }
}

/// When the countdown to the next reconnection shows a different number.
fn refresh_at(snapshot: &Snapshot, now: Instant) -> Option<Instant> {
    snapshot
        .services
        .iter()
        .filter_map(|s| match s.connection {
            Connection::Disconnected { retry_at, .. } => retry_at.checked_duration_since(now),
            _ => None,
        })
        .filter(|wait| !wait.is_zero())
        .map(|wait| {
            let tick = wait.subsec_nanos();
            now + if tick == 0 {
                Duration::from_secs(1)
            } else {
                Duration::from_nanos(tick.into())
            }
        })
        .min()
}

/// The host (and port, if unusual) the owner would recognize.
fn host(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => url.to_string(),
    }
}

/// Text from a service, made fit for a menu: one line, at most `max`
/// characters, with no characters that are invisible or reorder text.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut chars = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|&c| !crate::page::is_hidden(c));
    for c in chars.by_ref().take(max) {
        out.push(c);
    }
    if chars.next().is_some() {
        out.truncate(out.trim_end().len());
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use moil_agent::BundleStatus;
    use moil_proto::{BundleInfo, JobId};

    use super::*;
    use crate::demo::service_id;

    fn hash(c: char) -> BundleHash {
        c.to_string().repeat(64).parse().unwrap()
    }

    fn bundle(name: &str, version: &str, h: char, approved: bool) -> BundleStatus {
        BundleStatus {
            info: BundleInfo {
                hash: hash(h),
                name: name.into(),
                version: version.into(),
                description: None,
            },
            approved,
        }
    }

    fn klisi() -> ServiceStatus {
        ServiceStatus {
            id: service_id("klisi.example.com"),
            name: "klisi".into(),
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
            connection: Connection::Connected,
            bundles: vec![
                bundle("transcribe", "0.2.0", 'a', true),
                bundle("transcribe", "0.3.0", 'b', false),
            ],
            insecure: false,
        }
    }

    fn lab(now: Instant) -> ServiceStatus {
        ServiceStatus {
            id: service_id("localhost-8787"),
            name: "lab".into(),
            base_url: "http://localhost:8787/moil".parse().unwrap(),
            connection: Connection::Disconnected {
                error: "connection refused".into(),
                retry_at: now + Duration::from_millis(11_400),
            },
            bundles: vec![],
            insecure: true,
        }
    }

    fn job(phase: Phase, fraction: Option<f64>) -> JobStatus {
        JobStatus {
            service: service_id("klisi.example.com"),
            job_id: JobId::new("j_42").unwrap(),
            attempt: 1,
            title: Some("Weekly sync".into()),
            bundle_name: "transcribe".into(),
            phase,
            fraction,
            message: Some("diarizing".into()),
            started_at: Instant::now(),
        }
    }

    struct Fixture {
        now: Instant,
        deferred: HashSet<(ServiceId, BundleHash)>,
        open_at_login: Option<bool>,
        quitting: bool,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                now: Instant::now(),
                deferred: HashSet::new(),
                open_at_login: Some(true),
                quitting: false,
            }
        }

        fn view(&self, snapshot: &Snapshot) -> TrayView {
            TrayView::new(
                snapshot,
                &ViewContext {
                    now: self.now,
                    open_at_login: self.open_at_login,
                    quitting: self.quitting,
                    deferred: &self.deferred,
                },
            )
        }
    }

    fn lines(menu: &MenuModel) -> Vec<String> {
        fn walk(entries: &[Entry], depth: usize, out: &mut Vec<String>) {
            for entry in entries {
                let indent = "  ".repeat(depth);
                match entry {
                    Entry::Text(label) => out.push(format!("{indent}{label}")),
                    Entry::Item { label, .. } => out.push(format!("{indent}[{label}]")),
                    Entry::Check {
                        label,
                        checked,
                        enabled,
                        ..
                    } => out.push(format!(
                        "{indent}[{}{label}]{}",
                        if *checked { "✓ " } else { "" },
                        if *enabled { "" } else { " (disabled)" }
                    )),
                    Entry::Submenu { label, entries } => {
                        out.push(format!("{indent}{label} ▸"));
                        walk(entries, depth + 1, out);
                    }
                    Entry::Separator => out.push(format!("{indent}---")),
                }
            }
        }
        let mut out = Vec::new();
        walk(&menu.entries, 0, &mut out);
        out
    }

    #[test]
    fn the_menu_lays_out_status_services_and_controls() {
        let fx = Fixture::new();
        let snapshot = Snapshot {
            paused: false,
            services: vec![klisi(), lab(fx.now)],
            job: Some(job(Phase::Running, Some(0.42))),
        };
        assert_eq!(
            lines(&fx.view(&snapshot).menu),
            [
                "Running “Weekly sync” for klisi — 42%",
                "transcribe: diarizing",
                "[Stop Current Job]",
                "---",
                "klisi — klisi.example.com ▸",
                "  https://klisi.example.com/moil",
                "  Connected",
                "  ---",
                "  [transcribe 0.2.0 — approved]",
                "  [Review transcribe 0.3.0…]",
                "  ---",
                "  [Unpair klisi…]",
                "Connected",
                "[Review transcribe 0.3.0…]",
                "---",
                "lab — localhost:8787 ▸",
                "  http://localhost:8787/moil",
                "  Offline: connection refused. Retrying in 12s",
                "  Unencrypted connection (http)",
                "  ---",
                "  Offers no job bundles",
                "  ---",
                "  [Unpair lab…]",
                "Offline: connection refused. Retrying in 12s",
                "---",
                "[Pause]",
                "[Pair with a Service…]",
                "---",
                "[✓ Open at Login]",
                "[Quit moil]",
            ]
        );
    }

    #[test]
    fn items_do_what_they_say() {
        let fx = Fixture::new();
        let snapshot = Snapshot {
            paused: true,
            services: vec![klisi()],
            job: None,
        };
        let menu = fx.view(&snapshot).menu;
        assert_eq!(
            menu.action("Review transcribe 0.3.0…"),
            Some(&Action::Review {
                service: service_id("klisi.example.com"),
                hash: hash('b')
            })
        );
        assert_eq!(
            menu.action("transcribe 0.2.0 — approved"),
            Some(&Action::Review {
                service: service_id("klisi.example.com"),
                hash: hash('a')
            })
        );
        assert_eq!(menu.action("Resume"), Some(&Action::SetPaused(false)));
        assert_eq!(menu.action("Pause"), None);
        assert_eq!(menu.action("Stop Current Job"), None, "no job, no stop");
        assert_eq!(
            menu.action("Unpair klisi…"),
            Some(&Action::Unpair(service_id("klisi.example.com")))
        );
        assert_eq!(menu.action("Quit moil"), Some(&Action::Quit));
    }

    #[test]
    fn the_status_line_follows_the_job() {
        let fx = Fixture::new();
        let status = |paused, job| {
            lines(
                &fx.view(&Snapshot {
                    paused,
                    services: vec![klisi()],
                    job,
                })
                .menu,
            )[0]
            .clone()
        };
        assert_eq!(status(false, None), "Idle — waiting for jobs");
        assert_eq!(status(true, None), "Paused — taking no new jobs");
        let running = |phase, fraction| status(false, Some(job(phase, fraction)));
        assert_eq!(
            running(Phase::Preparing, None),
            "Running “Weekly sync” for klisi — preparing"
        );
        assert_eq!(
            running(Phase::Downloading, None),
            "Running “Weekly sync” for klisi — downloading inputs"
        );
        assert_eq!(
            running(Phase::Running, None),
            "Running “Weekly sync” for klisi"
        );
        assert_eq!(
            running(Phase::Running, Some(0.999)),
            "Running “Weekly sync” for klisi — 99%"
        );
        assert_eq!(
            running(Phase::Uploading, Some(1.0)),
            "Running “Weekly sync” for klisi — uploading results"
        );

        let mut untitled = job(Phase::Running, Some(0.1));
        untitled.title = None;
        assert_eq!(
            status(false, Some(untitled)),
            "Running “transcribe” for klisi — 10%"
        );

        let paused = fx.view(&Snapshot {
            paused: true,
            services: vec![klisi()],
            job: Some(job(Phase::Running, Some(0.5))),
        });
        assert!(lines(&paused.menu).contains(&"Paused: no new jobs after this one".to_owned()));
        assert_eq!(
            paused.icon.activity,
            Activity::Running,
            "the job carries on"
        );
    }

    #[test]
    fn the_title_shows_progress_only_while_the_script_runs() {
        let fx = Fixture::new();
        let title = |job| {
            fx.view(&Snapshot {
                paused: false,
                services: vec![],
                job,
            })
            .title
        };
        assert_eq!(
            title(Some(job(Phase::Running, Some(0.42)))),
            Some("42%".into())
        );
        assert_eq!(title(Some(job(Phase::Running, None))), None);
        assert_eq!(title(Some(job(Phase::Preparing, None))), None);
        assert_eq!(title(None), None);
    }

    #[test]
    fn attention_means_the_owner_is_needed() {
        let mut fx = Fixture::new();
        let attention = |fx: &Fixture, services| {
            fx.view(&Snapshot {
                paused: false,
                services,
                job: None,
            })
            .icon
            .attention
        };
        assert!(attention(&fx, vec![klisi()]), "a review waits");

        let mut reviewed = klisi();
        reviewed.bundles.retain(|b| b.approved);
        assert!(
            !attention(&fx, vec![reviewed.clone(), lab(fx.now)]),
            "offline isn't the owner's problem"
        );

        for lost in [Connection::Unpaired, Connection::Replaced] {
            let mut service = reviewed.clone();
            service.connection = lost;
            assert!(attention(&fx, vec![service]));
        }

        fx.deferred
            .insert((service_id("klisi.example.com"), hash('b')));
        assert!(!attention(&fx, vec![klisi()]), "put off with Not now");
        let menu = fx
            .view(&Snapshot {
                paused: false,
                services: vec![klisi()],
                job: None,
            })
            .menu;
        assert!(
            menu.action("Review transcribe 0.3.0…").is_some(),
            "but still in the menu"
        );
    }

    #[test]
    fn activity_prefers_running_then_paused() {
        let fx = Fixture::new();
        let activity = |paused, job| {
            fx.view(&Snapshot {
                paused,
                services: vec![],
                job,
            })
            .icon
            .activity
        };
        assert_eq!(activity(false, None), Activity::Idle);
        assert_eq!(activity(true, None), Activity::Paused);
        assert_eq!(
            activity(false, Some(job(Phase::Preparing, None))),
            Activity::Running
        );
    }

    #[test]
    fn an_unpaired_service_offers_pairing_again() {
        let fx = Fixture::new();
        let mut service = klisi();
        service.connection = Connection::Unpaired;
        let menu = fx
            .view(&Snapshot {
                paused: false,
                services: vec![service],
                job: None,
            })
            .menu;
        assert!(lines(&menu).contains(&"Removed this Mac; pair again to use it".to_owned()));
        assert_eq!(
            menu.action("Pair with klisi Again…"),
            Some(&Action::PairAgain(
                "https://klisi.example.com/moil".parse().unwrap()
            ))
        );
    }

    #[test]
    fn open_at_login_reflects_what_can_be_changed() {
        let mut fx = Fixture::new();
        let snapshot = Snapshot::default();
        let check = |fx: &Fixture| {
            lines(&fx.view(&snapshot).menu)
                .into_iter()
                .find(|l| l.contains("Open at Login"))
                .unwrap()
        };
        assert_eq!(check(&fx), "[✓ Open at Login]");
        fx.open_at_login = Some(false);
        assert_eq!(check(&fx), "[Open at Login]");
        fx.open_at_login = None;
        assert_eq!(check(&fx), "[Open at Login] (disabled)");
        assert!(
            lines(&fx.view(&snapshot).menu).contains(&"Not paired with any service".to_owned())
        );
    }

    #[test]
    fn quitting_leaves_nothing_to_click() {
        let mut fx = Fixture::new();
        fx.quitting = true;
        let view = fx.view(&Snapshot {
            paused: false,
            services: vec![klisi()],
            job: Some(job(Phase::Running, Some(0.3))),
        });
        assert_eq!(lines(&view.menu), ["Stopping the job and quitting…"]);
    }

    #[test]
    fn progress_keeps_the_shape_and_new_reviews_change_it() {
        let fx = Fixture::new();
        let at = |fraction, services| {
            fx.view(&Snapshot {
                paused: false,
                services,
                job: Some(job(Phase::Running, Some(fraction))),
            })
            .menu
        };
        let before = at(0.41, vec![klisi()]);
        let after = at(0.42, vec![klisi()]);
        assert_ne!(before, after);
        assert!(before.same_shape(&after));

        let mut more = klisi();
        more.bundles.push(bundle("summarize", "0.1.0", 'c', false));
        assert!(!before.same_shape(&at(0.42, vec![more])));

        let mut approved = klisi();
        approved.bundles[1].approved = true;
        assert!(!before.same_shape(&at(0.42, vec![approved])));
    }

    #[test]
    fn countdowns_refresh_when_their_number_changes() {
        let fx = Fixture::new();
        let view = fx.view(&Snapshot {
            paused: false,
            services: vec![klisi(), lab(fx.now)],
            job: None,
        });
        assert_eq!(view.refresh_at, Some(fx.now + Duration::from_millis(400)));
        assert!(
            fx.view(&Snapshot {
                paused: false,
                services: vec![klisi()],
                job: None
            })
            .refresh_at
            .is_none()
        );

        let mut due = lab(fx.now);
        due.connection = Connection::Disconnected {
            error: "timed out".into(),
            retry_at: fx.now,
        };
        let view = fx.view(&Snapshot {
            paused: false,
            services: vec![due],
            job: None,
        });
        assert_eq!(view.refresh_at, None);
        assert!(lines(&view.menu).contains(&"Offline: timed out. Retrying now".to_owned()));
    }

    #[test]
    fn service_text_cannot_disguise_the_menu() {
        let fx = Fixture::new();
        let mut service = klisi();
        service.name = "kli\u{202e}si\nOK".into();
        service.bundles[1].info.name = format!("{}x", "long".repeat(20));
        let menu = fx
            .view(&Snapshot {
                paused: false,
                services: vec![service],
                job: None,
            })
            .menu;
        let lines = lines(&menu);
        assert!(
            lines.contains(&"klisi OK — klisi.example.com ▸".to_owned()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&format!("[Review {}… 0.3.0…]", "long".repeat(8))),
            "{lines:#?}"
        );
    }
}
