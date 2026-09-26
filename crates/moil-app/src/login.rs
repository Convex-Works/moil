//! Open at Login.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use auto_launch::AutoLaunch;

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
        let item = self
            .0
            .as_ref()
            .ok_or("Open at Login works only for the installed app.")?;
        let result = if enabled {
            item.enable()
        } else {
            item.disable()
        };
        result.map_err(|e| format!("Couldn't change Open at Login: {e}"))
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
