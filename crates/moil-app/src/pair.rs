//! The pair window: the owner enters a service's address (or a `moil://`
//! link fills it in), confirms a code on the service, and the machine is
//! paired (spec §4).
//!
//! Nothing starts until the owner presses Continue, even when a link
//! supplied the address: a link can come from anywhere.

use std::time::Instant;

use maud::{Markup, html};
use url::Url;

use crate::backend::PairingCode;
use crate::format;
use crate::ipc::PairMessage;
use crate::links::{browser_url, service_address};
use crate::page::{self, icon, link, visible};

/// One pair window and where the owner is in pairing.
#[derive(Debug)]
pub struct PairWindow {
    step: Step,
    /// Increases with every pairing started here, so that the outcome of a
    /// pairing the owner abandoned is recognized and ignored.
    attempt: u64,
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// Asking for the address. `from_link` when a link supplied it.
    Enter {
        address: String,
        from_link: bool,
        error: Option<String>,
    },
    /// Checking that the address is a moil service and asking it for a code.
    Starting {
        url: Url,
    },
    /// Waiting for the owner to confirm the code on the service.
    Confirming {
        url: Url,
        code: PairingCode,
    },
    Paired {
        service_name: String,
    },
    Failed {
        url: Url,
        error: String,
    },
}

/// What the app should do next for a pair window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairEffect {
    /// Start pairing with the service at `url`; report back tagged with
    /// `attempt`.
    Start { url: Url, attempt: u64 },
    /// Open the service's confirmation page in the browser.
    OpenBrowser(Url),
    /// Abandon any pairing in progress and close the window.
    Close,
}

impl PairWindow {
    /// A window asking for an address, pre-filled with `address` if given.
    pub fn new(address: Option<&Url>) -> Self {
        Self {
            step: Step::Enter {
                address: address.map(Url::to_string).unwrap_or_default(),
                from_link: false,
                error: None,
            },
            attempt: 0,
        }
    }

    /// A window for a `moil://pair` link: the address filled in, and the
    /// owner asked whether they meant it.
    pub fn from_link(url: &Url) -> Self {
        let mut window = Self::new(None);
        window.link(url);
        window
    }

    /// A link arrived while the window is open. It replaces the address
    /// unless a pairing is under way, which it doesn't interrupt.
    pub fn link(&mut self, url: &Url) {
        if !self.is_pairing() {
            self.step = Step::Enter {
                address: url.to_string(),
                from_link: true,
                error: None,
            };
        }
    }

    /// Whether a pairing is under way, which closing the window abandons.
    pub fn is_pairing(&self) -> bool {
        matches!(self.step, Step::Starting { .. } | Step::Confirming { .. })
    }

    pub fn title(&self) -> &'static str {
        "Pair with a Service"
    }

    pub fn on_message(&mut self, message: PairMessage) -> Option<PairEffect> {
        match (message, &self.step) {
            (PairMessage::Close {}, _) => Some(PairEffect::Close),
            (PairMessage::Start { url }, Step::Enter { from_link, .. }) => {
                match service_address(&url) {
                    Ok(parsed) => {
                        self.attempt += 1;
                        self.step = Step::Starting {
                            url: parsed.clone(),
                        };
                        Some(PairEffect::Start {
                            url: parsed,
                            attempt: self.attempt,
                        })
                    }
                    Err(error) => {
                        self.step = Step::Enter {
                            address: url,
                            from_link: *from_link,
                            error: Some(error),
                        };
                        None
                    }
                }
            }
            (PairMessage::OpenBrowser {}, Step::Confirming { code, .. }) => {
                browser_url(&code.verification_uri_complete).map(PairEffect::OpenBrowser)
            }
            (PairMessage::Retry {}, Step::Failed { url, .. }) => {
                self.step = Step::Enter {
                    address: url.to_string(),
                    from_link: false,
                    error: None,
                };
                None
            }
            _ => None,
        }
    }

    /// The service answered with a code, or pairing couldn't start. On
    /// success the app opens the confirmation page right away.
    pub fn started(
        &mut self,
        attempt: u64,
        result: Result<PairingCode, String>,
    ) -> Option<PairEffect> {
        let Step::Starting { url } = &self.step else {
            return None;
        };
        if attempt != self.attempt {
            return None;
        }
        match result {
            Ok(code) => {
                let open =
                    browser_url(&code.verification_uri_complete).map(PairEffect::OpenBrowser);
                self.step = Step::Confirming {
                    url: url.clone(),
                    code,
                };
                open
            }
            Err(error) => {
                // Most failures here are about the address: let the owner fix it.
                self.step = Step::Enter {
                    address: url.to_string(),
                    from_link: false,
                    error: Some(error),
                };
                None
            }
        }
    }

    /// The owner confirmed or rejected the pairing, or the code expired.
    pub fn finished(&mut self, attempt: u64, result: Result<String, String>) {
        let Step::Confirming { url, .. } = &self.step else {
            return;
        };
        if attempt != self.attempt {
            return;
        }
        self.step = match result {
            Ok(service_name) => Step::Paired { service_name },
            Err(error) => Step::Failed {
                url: url.clone(),
                error,
            },
        };
    }

    /// The page for the current step. `now` says how long the code has left.
    pub fn render(&self, now: Instant) -> String {
        let body = match &self.step {
            Step::Enter {
                address,
                from_link,
                error,
            } => enter(address, *from_link, error.as_deref()),
            Step::Starting { url } => starting(url),
            Step::Confirming { code, .. } => confirming(code, now),
            Step::Paired { service_name } => paired(service_name),
            Step::Failed { error, .. } => failed(error),
        };
        page::document(self.title(), "dialog", body)
    }
}

fn enter(address: &str, from_link: bool, error: Option<&str>) -> Markup {
    html! {
        form data-action="start" {
            main.dialog {
                h1 { "Pair with a service" }
                p {
                    "Pairing lets a service offer this Mac jobs. "
                    "Nothing runs until you have reviewed and approved the code for it."
                }
                @if from_link {
                    div.callout.warn {
                        (icon::warning())
                        div {
                            strong { "A link asked to pair this Mac with the service below." }
                            p { "Continue only if you started pairing on that service yourself." }
                        }
                    }
                }
                div {
                    label for="address" { "Service address" }
                    input #address type="text" name="url" value=(address)
                        placeholder="https://klisi.example.com/moil"
                        autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false"
                        autofocus;
                }
                @if let Some(error) = error {
                    div.callout.bad { (icon::error()) div { p.message { (visible(error)) } } }
                } @else {
                    p.hint { "The service shows its address where it asks you to add a machine." }
                }
            }
            footer.actions {
                span.spacer {}
                button type="button" data-action="close" { "Cancel" }
                button.primary type="submit" { "Continue" }
            }
        }
    }
}

fn starting(url: &Url) -> Markup {
    html! {
        main.dialog {
            div.centered {
                span.spinner {}
                h1 { "Contacting " (visible(url.host_str().unwrap_or(url.as_str()))) "…" }
                p.muted { "Checking that it is a moil service and asking it for a pairing code." }
            }
        }
        footer.actions { span.spacer {} button data-action="close" { "Cancel" } }
    }
}

fn confirming(code: &PairingCode, now: Instant) -> Markup {
    let remaining = code.expires_at.saturating_duration_since(now);
    let page_url = browser_url(&code.verification_uri);
    let page_name = page_url
        .as_ref()
        .map(|u| format!("{}{}", u.host_str().unwrap_or_default(), u.path()))
        .unwrap_or_else(|| code.verification_uri.clone());
    html! {
        main.dialog {
            h1 { "Confirm on " (visible(&code.service_name)) }
            p { "Check that " (visible(&code.service_name)) " shows this code, then confirm there:" }
            div.user-code { (visible(&code.user_code)) }
            p {
                "Your browser should have opened "
                (link(&code.verification_uri_complete, visible(&page_name)))
                ". If it didn't, open it yourself and enter the code."
            }
            div.waiting {
                span.spinner {}
                span {
                    "Waiting for you to confirm. "
                    @if remaining.is_zero() {
                        "The code has expired."
                    } @else {
                        "The code expires in "
                        span data-countdown=(remaining.as_secs()) { (format::wait(remaining)) }
                        "."
                    }
                }
            }
        }
        footer.actions {
            span.spacer {}
            button data-action="close" { "Cancel" }
            button data-action="open_browser" { "Open Browser Again" }
        }
    }
}

fn paired(service_name: &str) -> Markup {
    html! {
        main.dialog {
            div.notice {
                span.notice-icon.good { (icon::check()) }
                h1 { "Paired with " (visible(service_name)) }
                p {
                    (visible(service_name)) " can now offer this Mac jobs. "
                    "moil will ask you to review each job bundle before it runs anything."
                }
            }
        }
        footer.actions { span.spacer {} button.primary data-action="close" autofocus { "Done" } }
    }
}

fn failed(error: &str) -> Markup {
    html! {
        main.dialog {
            div.notice {
                span.notice-icon.bad { (icon::error()) }
                h1 { "Pairing didn't finish" }
                p.message { (visible(error)) }
            }
        }
        footer.actions {
            span.spacer {}
            button data-action="close" { "Close" }
            button.primary data-action="retry" autofocus { "Try Again" }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::page::tests::assert_locked_down;

    fn code(now: Instant) -> PairingCode {
        PairingCode {
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://klisi.example.com/machines/pair".into(),
            verification_uri_complete: "https://klisi.example.com/machines/pair?code=WDJB-MJHT"
                .into(),
            service_name: "klisi".into(),
            expires_at: now + Duration::from_secs(600),
        }
    }

    fn start(window: &mut PairWindow, address: &str) -> u64 {
        match window.on_message(PairMessage::Start {
            url: address.into(),
        }) {
            Some(PairEffect::Start { attempt, .. }) => attempt,
            other => panic!("expected pairing to start, got {other:?}"),
        }
    }

    #[test]
    fn a_link_fills_in_the_address_but_waits_for_the_owner() {
        let url: Url = "https://klisi.example.com/moil".parse().unwrap();
        let window = PairWindow::from_link(&url);
        assert!(!window.is_pairing());
        let page = window.render(Instant::now());
        assert_locked_down(&page);
        assert!(page.contains(r#"value="https://klisi.example.com/moil""#));
        assert!(page.contains("A link asked to pair this Mac"));
        assert!(page.contains(r#"<form data-action="start">"#));
    }

    #[test]
    fn the_whole_flow() {
        let now = Instant::now();
        let mut window = PairWindow::new(None);
        assert!(!window.render(now).contains("A link asked"));

        let Some(PairEffect::Start { url, attempt }) = window.on_message(PairMessage::Start {
            url: " klisi.example.com/moil ".into(),
        }) else {
            panic!("pairing should start");
        };
        assert_eq!(url.as_str(), "https://klisi.example.com/moil");
        assert!(window.is_pairing());
        assert!(window.render(now).contains("Contacting klisi.example.com…"));

        // The code arrives: the browser opens on the confirmation page.
        assert_eq!(
            window.started(attempt, Ok(code(now))),
            Some(PairEffect::OpenBrowser(
                "https://klisi.example.com/machines/pair?code=WDJB-MJHT"
                    .parse()
                    .unwrap()
            ))
        );
        let page = window.render(now + Duration::from_secs(30));
        assert!(page.contains(r#"<div class="user-code">WDJB-MJHT</div>"#));
        assert!(page.contains(
            r#"<a href="https://klisi.example.com/machines/pair?code=WDJB-MJHT">klisi.example.com/machines/pair</a>"#
        ));
        assert!(page.contains(r#"The code expires in <span data-countdown="570">10 min</span>."#));
        assert!(matches!(
            window.on_message(PairMessage::OpenBrowser {}),
            Some(PairEffect::OpenBrowser(_))
        ));

        window.finished(attempt, Ok("klisi".into()));
        assert!(!window.is_pairing());
        let page = window.render(now);
        assert!(page.contains("Paired with klisi"));
        assert!(page.contains("review each job bundle"));
    }

    #[test]
    fn a_bad_address_is_explained_in_place() {
        let mut window = PairWindow::new(None);
        assert_eq!(
            window.on_message(PairMessage::Start {
                url: "ftp://klisi.example.com".into()
            }),
            None
        );
        let page = window.render(Instant::now());
        assert!(page.contains("moil services have https addresses, not ftp:// ones."));
        assert!(page.contains(r#"value="ftp://klisi.example.com""#));
    }

    #[test]
    fn a_service_that_refuses_to_start_sends_the_owner_back_to_the_address() {
        let mut window = PairWindow::new(None);
        let attempt = start(&mut window, "https://klisi.example.com/wrong");
        window.started(
            attempt,
            Err("https://klisi.example.com/wrong isn't a moil service: 404".into()),
        );
        let page = window.render(Instant::now());
        assert!(page.contains("isn't a moil service: 404"));
        assert!(page.contains(r#"value="https://klisi.example.com/wrong""#));
    }

    #[test]
    fn rejection_offers_to_try_again() {
        let now = Instant::now();
        let mut window = PairWindow::new(None);
        let attempt = start(&mut window, "https://klisi.example.com/moil");
        window.started(attempt, Ok(code(now)));
        window.finished(
            attempt,
            Err("the pairing was rejected on the service".into()),
        );
        let page = window.render(now);
        assert!(page.contains("Pairing didn't finish"));
        assert!(page.contains("the pairing was rejected on the service"));
        assert_eq!(window.on_message(PairMessage::Retry {}), None);
        assert!(
            window
                .render(now)
                .contains(r#"value="https://klisi.example.com/moil""#)
        );
    }

    #[test]
    fn outcomes_of_abandoned_pairings_are_ignored() {
        let now = Instant::now();
        let mut window = PairWindow::new(None);
        let first = start(&mut window, "https://a.example/moil");
        window.started(first, Err("timed out".into()));
        let second = start(&mut window, "https://b.example/moil");
        assert_ne!(first, second);

        assert_eq!(window.started(first, Ok(code(now))), None);
        assert!(window.render(now).contains("Contacting b.example…"));
        window.started(second, Ok(code(now)));
        window.finished(first, Ok("a".into()));
        assert!(window.is_pairing(), "still waiting for the second pairing");
    }

    #[test]
    fn links_do_not_interrupt_a_pairing() {
        let now = Instant::now();
        let mut window = PairWindow::new(None);
        let attempt = start(&mut window, "https://klisi.example.com/moil");
        window.started(attempt, Ok(code(now)));
        window.link(&"https://evil.example/moil".parse().unwrap());
        assert!(window.render(now).contains("WDJB-MJHT"));
    }

    #[test]
    fn only_web_pages_open_in_the_browser() {
        let now = Instant::now();
        let mut window = PairWindow::new(None);
        let attempt = start(&mut window, "https://klisi.example.com/moil");
        let mut sneaky = code(now);
        sneaky.verification_uri_complete = "file:///Applications/Calculator.app".into();
        assert_eq!(window.started(attempt, Ok(sneaky)), None);
        assert_eq!(window.on_message(PairMessage::OpenBrowser {}), None);
    }
}
