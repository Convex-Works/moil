//! The window that asks the owner to confirm unpairing a service. The
//! service forgets this machine, so using it again means pairing again;
//! the service's folder stays, approvals and all (spec/machine.md §3.2).

use maud::html;
use moil_agent::{ServiceId, ServiceStatus};
use url::Url;

use crate::format;
use crate::ipc::ConfirmMessage;
use crate::page::{self, link, visible};

#[derive(Debug)]
pub struct UnpairWindow {
    service: ServiceId,
    name: String,
    base_url: Url,
    approvals: usize,
    busy: bool,
    error: Option<String>,
    /// The page shown no longer matches the state.
    stale: bool,
}

/// What the app should do next for an unpair window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpairEffect {
    Unpair(ServiceId),
    Close,
}

impl UnpairWindow {
    pub fn new(service: &ServiceStatus) -> Self {
        Self {
            service: service.id.clone(),
            name: service.name.clone(),
            base_url: service.base_url.clone(),
            approvals: service.bundles.iter().filter(|b| b.approved).count(),
            busy: false,
            error: None,
            stale: false,
        }
    }

    /// Whether the page shown is out of date and should be rendered again.
    /// Clicking Unpair doesn't make it so: the page shows that it is busy.
    pub fn needs_render(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }

    pub fn service(&self) -> &ServiceId {
        &self.service
    }

    pub fn title(&self) -> String {
        format!("Unpair {}", format::one_line(&self.name, 32))
    }

    pub fn on_message(&mut self, message: ConfirmMessage) -> Option<UnpairEffect> {
        match message {
            ConfirmMessage::Close {} => Some(UnpairEffect::Close),
            ConfirmMessage::Confirm {} if self.busy => None,
            ConfirmMessage::Confirm {} => {
                self.busy = true;
                self.error = None;
                Some(UnpairEffect::Unpair(self.service.clone()))
            }
        }
    }

    /// The agent finished unpairing: the window closes, or says why not.
    pub fn finished(&mut self, result: Result<(), String>) -> Option<UnpairEffect> {
        if !std::mem::replace(&mut self.busy, false) {
            return None;
        }
        match result {
            Ok(()) => Some(UnpairEffect::Close),
            Err(error) => {
                self.error = Some(error);
                self.stale = true;
                None
            }
        }
    }

    pub fn render(&self) -> String {
        let name = visible(&self.name);
        let body = html! {
            main.dialog {
                h1 { "Unpair " (name) "?" }
                p {
                    (name) " at " (link(self.base_url.as_str(), visible(self.base_url.as_str())))
                    " will no longer be able to send jobs to this Mac. moil tells " (name)
                    ", then forgets its token."
                    @match self.approvals {
                        0 => {},
                        1 => " The bundle you approved for it stays approved, for when you pair again.",
                        n => {
                            " The " (format::count(n, "bundle")) " you approved for it stay \
                             approved, for when you pair again."
                        },
                    }
                }
                p.hint {
                    "To forget " (name) " altogether once it's unpaired, choose Open moil Folder \
                     in the menu and delete " code { "services/" (self.service.as_str()) } "."
                }
                @if let Some(error) = &self.error {
                    div.callout.bad { (page::icon::error()) div { p.message { (visible(error)) } } }
                }
            }
            footer.actions {
                span.spacer {}
                button data-action="close" disabled[self.busy] { "Cancel" }
                button.danger data-action="confirm" data-busy="Unpairing…" disabled[self.busy] {
                    @if self.busy { "Unpairing…" } @else { "Unpair" }
                }
            }
        };
        page::document(&self.title(), "dialog", body)
    }
}

#[cfg(test)]
mod tests {
    use moil_agent::{BundleStatus, Connection};
    use moil_proto::BundleInfo;

    use super::*;
    use crate::page::tests::assert_locked_down;

    fn klisi() -> ServiceStatus {
        let bundle = |hash: &str, approved| BundleStatus {
            info: BundleInfo {
                hash: hash.parse().unwrap(),
                name: "transcribe".into(),
                version: "0.2.0".into(),
                description: None,
            },
            approved,
        };
        ServiceStatus {
            id: "klisi.example.com".parse().unwrap(),
            name: "klisi".into(),
            base_url: "https://klisi.example.com/moil".parse().unwrap(),
            connection: Connection::Connected,
            bundles: vec![
                bundle(&"a".repeat(64), true),
                bundle(&"b".repeat(64), true),
                bundle(&"c".repeat(64), false),
            ],
            insecure: false,
        }
    }

    #[test]
    fn says_what_unpairing_forgets_and_what_it_keeps() {
        let page = UnpairWindow::new(&klisi()).render();
        assert_locked_down(&page);
        assert!(page.contains("Unpair klisi?"));
        assert!(page.contains(r#"<a href="https://klisi.example.com/moil">"#));
        assert!(page.contains("forgets its token."));
        assert!(page.contains("The 2 bundles you approved for it stay approved"));
        assert!(
            page.contains("choose Open moil Folder in the menu and delete <code>services/klisi.example.com</code>"),
            "where to forget it altogether"
        );
    }

    #[test]
    fn confirming_unpairs_once() {
        let mut window = UnpairWindow::new(&klisi());
        assert_eq!(
            window.on_message(ConfirmMessage::Confirm {}),
            Some(UnpairEffect::Unpair("klisi.example.com".parse().unwrap()))
        );
        assert_eq!(window.on_message(ConfirmMessage::Confirm {}), None);
        assert!(
            !window.needs_render(),
            "the page shows it is busy by itself"
        );
        assert!(window.render().contains("Unpairing…"));
        assert_eq!(window.finished(Ok(())), Some(UnpairEffect::Close));
    }

    #[test]
    fn a_failure_is_shown_and_can_be_retried() {
        let mut window = UnpairWindow::new(&klisi());
        window.on_message(ConfirmMessage::Confirm {});
        assert_eq!(
            window.finished(Err("couldn't delete the token".into())),
            None
        );
        assert!(window.render().contains("couldn't delete the token"));
        assert!(window.on_message(ConfirmMessage::Confirm {}).is_some());
    }
}
