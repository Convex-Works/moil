//! Open at Login.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use auto_launch::AutoLaunch;

/// Why Open at Login can't be told or changed from here.
const ONLY_INSTALLED: &str = "Open at Login works only for the installed app.";

/// Whether the app opens when the owner logs in.
pub trait LoginItem {
    /// `None` when it can't be changed from here.
    fn enabled(&self) -> Option<bool>;

    fn set_enabled(&self, enabled: bool) -> Result<(), String>;
}

/// The system's login items, for the installed app. On macOS that is the
/// app bundle, registered with `SMAppService`, which works only when
/// running from `Moil.app`.
#[derive(Debug)]
pub struct SystemLoginItem(Option<AutoLaunch>);

impl SystemLoginItem {
    pub fn new() -> Self {
        let exe = std::env::current_exe().ok();
        Self(exe.as_deref().and_then(system_auto_launch))
    }
}

#[cfg(target_os = "macos")]
fn system_auto_launch(exe: &Path) -> Option<AutoLaunch> {
    let bundle = app_bundle(exe)?;
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name("Moil")
        .set_app_path(&bundle.to_string_lossy())
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::SMAppService)
        .build()
        .ok()
}

#[cfg(not(target_os = "macos"))]
fn system_auto_launch(exe: &Path) -> Option<AutoLaunch> {
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name("moil")
        .set_app_path(&exe.to_string_lossy())
        .build()
        .ok()
}

impl LoginItem for SystemLoginItem {
    fn enabled(&self) -> Option<bool> {
        self.0.as_ref()?.is_enabled().ok()
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        let item = self.0.as_ref().ok_or(ONLY_INSTALLED)?;
        let result = if enabled {
            item.enable()
        } else {
            item.disable()
        };
        result.map_err(|e| format!("Couldn't change Open at Login: {e}"))
    }
}

/// Answers `moil login-item` (spec/machine.md §3.2) from the item the menu
/// shows: whether the app opens at login, once turned on or off if `set`
/// says to, or why that can't be told or changed.
pub fn answer(item: &dyn LoginItem, set: Option<bool>) -> Result<bool, String> {
    let enabled = item.enabled();
    match set {
        Some(set) if enabled != Some(set) => {
            item.set_enabled(set)?;
            // As the system now has it, which may still need the owner's
            // approval in System Settings.
            item.enabled().ok_or_else(|| ONLY_INSTALLED.to_owned())
        }
        _ => enabled.ok_or_else(|| ONLY_INSTALLED.to_owned()),
    }
}

/// A login item whose state is remembered for a while once read. Asking
/// the system (`SMAppService`) takes a couple of milliseconds, and the
/// tray asks every time it redraws, which a job's progress makes it do
/// often. A change made here shows at once; one made in System Settings
/// shows within `max_age`.
#[derive(Debug)]
pub struct Cached<L> {
    item: L,
    max_age: Duration,
    known: Cell<Option<(Option<bool>, Instant)>>,
}

impl<L: LoginItem> Cached<L> {
    pub fn new(item: L, max_age: Duration) -> Self {
        Self {
            item,
            max_age,
            known: Cell::new(None),
        }
    }
}

impl<L: LoginItem> LoginItem for Cached<L> {
    fn enabled(&self) -> Option<bool> {
        match self.known.get() {
            Some((enabled, at)) if at.elapsed() < self.max_age => enabled,
            _ => {
                let enabled = self.item.enabled();
                self.known.set(Some((enabled, Instant::now())));
                enabled
            }
        }
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.known.set(None);
        self.item.set_enabled(enabled)
    }
}

/// An Open at Login setting that only lives in memory, so the demo never
/// touches the system's login items.
#[derive(Debug, Default)]
pub struct DemoLoginItem(Cell<bool>);

impl LoginItem for DemoLoginItem {
    fn enabled(&self) -> Option<bool> {
        Some(self.0.get())
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.0.set(enabled);
        Ok(())
    }
}

/// The `.app` bundle an executable runs from, if it runs from one:
/// `…/Moil.app/Contents/MacOS/moil-app` → `…/Moil.app`.
pub fn app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    is_bundle.then(|| bundle.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A login item that counts how often the system is asked.
    #[derive(Debug, Default)]
    struct Counting {
        enabled: Cell<bool>,
        asked: Cell<usize>,
    }

    impl LoginItem for &Counting {
        fn enabled(&self) -> Option<bool> {
            self.asked.set(self.asked.get() + 1);
            Some(self.enabled.get())
        }

        fn set_enabled(&self, enabled: bool) -> Result<(), String> {
            self.enabled.set(enabled);
            Ok(())
        }
    }

    #[test]
    fn the_system_is_asked_once_until_something_changes() {
        let system = Counting::default();
        let item = Cached::new(&system, Duration::from_secs(60));
        for _ in 0..100 {
            assert_eq!(item.enabled(), Some(false));
        }
        assert_eq!(system.asked.get(), 1, "one redraw after another");

        item.set_enabled(true).unwrap();
        assert_eq!(item.enabled(), Some(true), "a change here shows at once");
        assert_eq!(system.asked.get(), 2);

        // A change made elsewhere shows once what's known is old enough.
        let item = Cached::new(&system, Duration::ZERO);
        system.enabled.set(false);
        assert_eq!(item.enabled(), Some(false));
        assert_eq!(system.asked.get(), 3);
    }

    #[test]
    fn the_command_sees_and_changes_what_the_menu_does() {
        let system = Counting::default();
        let item = Cached::new(&system, Duration::from_secs(60));
        assert_eq!(answer(&item, None), Ok(false));
        assert_eq!(answer(&item, Some(true)), Ok(true));
        assert!(system.enabled.get());
        assert_eq!(item.enabled(), Some(true), "the menu shows it at once");
        let asked = system.asked.get();
        assert_eq!(answer(&item, Some(true)), Ok(true), "already on");
        assert_eq!(system.asked.get(), asked, "nothing to ask the system");
        assert_eq!(answer(&item, Some(false)), Ok(false));
        assert!(!system.enabled.get());
    }

    #[test]
    fn the_command_says_why_it_cant() {
        let loose = SystemLoginItem(None);
        for set in [None, Some(true), Some(false)] {
            assert_eq!(
                answer(&loose, set),
                Err("Open at Login works only for the installed app.".into())
            );
        }
    }

    #[test]
    fn only_executables_inside_an_app_bundle_have_one() {
        assert_eq!(
            app_bundle(Path::new("/Applications/Moil.app/Contents/MacOS/moil-app")),
            Some(PathBuf::from("/Applications/Moil.app"))
        );
        for loose in [
            "/Users/ada/moil/target/debug/moil-app",
            "/Applications/Moil.app/Contents/Resources/moil-app",
            "/Applications/Moil/Contents/MacOS/moil-app",
            "moil-app",
        ] {
            assert_eq!(app_bundle(Path::new(loose)), None, "{loose}");
        }
    }
}
