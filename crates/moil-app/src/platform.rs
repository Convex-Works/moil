//! The little that differs between platforms.

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

/// Opens a web page in the owner's browser. Only `http` and `https` URLs
/// get here (see [`crate::links::browser_url`]).
pub fn open_in_browser(url: &Url) {
    if let Err(e) = open::that_detached(url.as_str()) {
        tracing::warn!("couldn't open {url} in the browser: {e}");
    }
}
