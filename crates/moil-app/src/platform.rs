//! The little that differs between platforms.

use std::io;
use std::path::Path;
use std::process::Command;

use tao::event_loop::EventLoop;
use tray_icon::menu::Menu;
use url::Url;

/// Sets the event loop up for a menu-bar app: on macOS, no Dock icon and
/// no app menu.
pub fn configure<T: 'static>(event_loop: &mut EventLoop<T>) {
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = event_loop;
}

/// On macOS, an app menu that is never shown but gives windows the usual
/// editing shortcuts: without it, ⌘V can't paste an address into the pair
/// window. Keep the returned menu alive.
pub fn edit_shortcuts() -> Option<Menu> {
    #[cfg(target_os = "macos")]
    {
        use tray_icon::menu::{PredefinedMenuItem, Submenu};
        let edit = Submenu::with_items(
            "Edit",
            true,
            &[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::select_all(None),
            ],
        )
        .ok()?;
        let window =
            Submenu::with_items("Window", true, &[&PredefinedMenuItem::close_window(None)]).ok()?;
        let app = Submenu::new("moil", true);
        let menu = Menu::with_items(&[&app, &edit, &window]).ok()?;
        menu.init_for_nsapp();
        Some(menu)
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Makes the digits in the tray's title all as wide as a figure space, so
/// that [`format::fixed_percent`](crate::format::fixed_percent) keeps its
/// width as the number changes. The menu bar's font has proportional
/// digits, where a 1 is narrower than an 8.
pub fn use_tabular_digits(icon: &tray_icon::TrayIcon) {
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSFont, NSFontWeightRegular};
        let (Some(mtm), Some(item)) = (MainThreadMarker::new(), icon.ns_status_item()) else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };
        let size = NSFont::menuBarFontOfSize(0.0).pointSize();
        // SAFETY: reads a constant AppKit defines.
        let regular = unsafe { NSFontWeightRegular };
        let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, regular);
        button.setFont(Some(&font));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = icon;
}

/// Shows a file in the file manager: selected in a Finder window on macOS,
/// its folder elsewhere. A file that is gone shows its folder.
pub fn reveal(path: &Path) {
    let path = path.to_owned();
    // Waits for `open` to hand the request to Finder, so off the event loop.
    std::thread::spawn(move || {
        let revealed = if cfg!(target_os = "macos") && path.exists() {
            Command::new("open")
                .arg("-R")
                .arg(&path)
                .status()
                .and_then(|status| {
                    status
                        .success()
                        .then_some(())
                        .ok_or_else(|| io::Error::other(format!("open -R answered {status}")))
                })
        } else {
            open::that_detached(path.parent().unwrap_or(&path))
        };
        if let Err(e) = revealed {
            tracing::warn!("couldn't show {}: {e}", path.display());
        }
    });
}

/// Opens a folder in the file manager: a Finder window on macOS.
pub fn open_folder(path: &Path) {
    if let Err(e) = open::that_detached(path) {
        tracing::warn!("couldn't open {}: {e}", path.display());
    }
}

/// The signals that ask the app to quit: SIGTERM, as `kill`, launchd and
/// shutdown scripts send it, and SIGINT, Ctrl-C where it was started from
/// a terminal. Listening starts when it's made, so a signal that comes
/// while the app starts isn't missed.
pub struct QuitSignals {
    #[cfg(unix)]
    signals: [tokio::signal::unix::Signal; 2],
}

impl QuitSignals {
    /// Must be called within a Tokio runtime.
    pub fn listen() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                signals: [
                    signal(SignalKind::terminate())?,
                    signal(SignalKind::interrupt())?,
                ],
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// The next signal to quit.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let [terminate, interrupt] = &mut self.signals;
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Opens a web page in the owner's browser. Only `http` and `https` URLs
/// get here (see [`crate::links::browser_url`]).
pub fn open_in_browser(url: &Url) {
    if let Err(e) = open::that_detached(url.as_str()) {
        tracing::warn!("couldn't open {url} in the browser: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Sends the signals to this very process: listening, it lives on.
    #[cfg(unix)]
    #[tokio::test]
    async fn quit_signals_are_heard() {
        let mut signals = QuitSignals::listen().unwrap();
        for signal in ["-TERM", "-INT"] {
            let sent = Command::new("kill")
                .args([signal, &std::process::id().to_string()])
                .status()
                .unwrap();
            assert!(sent.success());
            let heard = tokio::time::timeout(Duration::from_secs(5), signals.recv()).await;
            assert!(heard.is_ok(), "kill {signal} went unheard");
        }
    }
}
