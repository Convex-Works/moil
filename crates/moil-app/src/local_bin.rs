//! The `moil` command on the owner's `PATH` (spec/machine.md §5): when the
//! app runs from its bundle, it links `~/.local/bin/moil` to the `moil`
//! beside it, so that shells find the command that matches the running
//! app.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::login::app_bundle;

/// What [`link`] found at the link's path, and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Linked {
    /// Nothing was there; now the link is.
    Created,
    /// A link into another `Moil.app` was there, and now points to the
    /// target instead.
    Replaced { previous: PathBuf },
    /// The link already pointed to the target.
    Unchanged,
    /// Something moil didn't put there is at the path, and stays: what it
    /// is.
    LeftAlone(String),
}

/// Links `~/.local/bin/moil` to the `moil` in the app's bundle, if the app
/// runs from one, and logs what it did or why it didn't. Does nothing when
/// the app doesn't run from a bundle, as under `cargo run`.
pub fn put_on_path() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return tracing::warn!("couldn't put moil on the PATH: {e}"),
    };
    let Some(bundle) = app_bundle(&exe) else {
        return;
    };
    if is_translocated(&bundle) {
        return tracing::info!(
            "didn't put moil on the PATH: macOS runs Moil.app from a temporary copy until it's \
             moved to Applications"
        );
    }
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return tracing::warn!("couldn't put moil on the PATH: $HOME isn't set");
    };
    let path = Path::new(&home).join(".local").join("bin").join("moil");
    let target = bundle.join("Contents").join("MacOS").join("moil");
    match link(&path, &target) {
        Ok(Linked::Created) => tracing::info!(
            "linked {} to {}, so that moil is on the PATH",
            path.display(),
            target.display()
        ),
        Ok(Linked::Replaced { previous }) => tracing::info!(
            "linked {} to {} instead of {}",
            path.display(),
            target.display(),
            previous.display()
        ),
        Ok(Linked::Unchanged) => {}
        Ok(Linked::LeftAlone(what)) => tracing::info!(
            "didn't link {} to {}: it is {what}, which moil didn't put there, so it stays",
            path.display(),
            target.display()
        ),
        Err(e) => tracing::warn!(
            "couldn't link {} to {}: {e}",
            path.display(),
            target.display()
        ),
    }
}

/// Makes `path` a symbolic link to `target`, only when nothing is at
/// `path`, or a symbolic link into a `Moil.app` is: never replaces anything
/// else. Creates the folder it goes in. Fails if `target` isn't a file.
pub fn link(path: &Path, target: &Path) -> io::Result<Linked> {
    if !target.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} isn't there", target.display()),
        ));
    }
    let previous = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
        Ok(meta) if meta.is_symlink() => {
            let points_to = fs::read_link(path)?;
            if points_to == target {
                return Ok(Linked::Unchanged);
            }
            if !is_into_moil_app(&points_to) {
                let what = format!("a link to {}", points_to.display());
                return Ok(Linked::LeftAlone(what));
            }
            Some(points_to)
        }
        Ok(meta) if meta.is_dir() => return Ok(Linked::LeftAlone("a folder".into())),
        Ok(_) => return Ok(Linked::LeftAlone("a file".into())),
    };
    let Some(previous) = previous else {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Fails, rather than replaces, if something got there meanwhile.
        std::os::unix::fs::symlink(target, path)?;
        return Ok(Linked::Created);
    };
    moil_agent::fs::replace_symlink(target, path)?;
    Ok(Linked::Replaced { previous })
}

/// Whether a link's destination is inside a `Moil.app`, wherever that is.
fn is_into_moil_app(points_to: &Path) -> bool {
    points_to.components().any(|c| match c {
        Component::Normal(name) => name
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("Moil.app")),
        _ => false,
    })
}

/// Whether macOS runs the bundle from the read-only copy it makes of an app
/// downloaded but not yet moved (App Translocation), which goes away.
fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|c| c.as_os_str() == "AppTranslocation")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    /// A Moil.app with its `moil`, and where the link goes.
    struct Setup {
        dir: TempDir,
        target: PathBuf,
        path: PathBuf,
    }

    impl Setup {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let target = app(dir.path(), "Applications/Moil.app");
            let path = dir.path().join("home/.local/bin/moil");
            Self { dir, target, path }
        }

        fn link(&self) -> Linked {
            link(&self.path, &self.target).unwrap()
        }

        fn points_to(&self) -> PathBuf {
            fs::read_link(&self.path).unwrap()
        }

        fn put(&self, what: impl FnOnce(&Path)) {
            fs::create_dir_all(self.path.parent().unwrap()).unwrap();
            what(&self.path);
        }
    }

    /// Makes `<root>/<bundle>/Contents/MacOS/moil` and returns its path.
    fn app(root: &Path, bundle: &str) -> PathBuf {
        let macos = root.join(bundle).join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let moil = macos.join("moil");
        fs::write(&moil, "#!/bin/sh\n").unwrap();
        moil
    }

    #[test]
    fn links_where_nothing_is() {
        let setup = Setup::new();
        assert_eq!(setup.link(), Linked::Created, "and makes ~/.local/bin");
        assert_eq!(setup.points_to(), setup.target);
        assert_eq!(setup.link(), Linked::Unchanged);
    }

    #[test]
    fn replaces_a_link_into_another_moil_app() {
        let setup = Setup::new();
        let old = app(setup.dir.path(), "Downloads/Moil.app");
        setup.put(|path| symlink(&old, path).unwrap());
        assert_eq!(setup.link(), Linked::Replaced { previous: old });
        assert_eq!(setup.points_to(), setup.target);

        // Even one that's gone, or relative.
        let gone = PathBuf::from("/Volumes/Moil 0.1/Moil.app/Contents/MacOS/moil");
        let relative = PathBuf::from("../../Applications/moil.APP/Contents/MacOS/moil");
        for previous in [gone, relative] {
            fs::remove_file(&setup.path).unwrap();
            symlink(&previous, &setup.path).unwrap();
            assert_eq!(setup.link(), Linked::Replaced { previous });
            assert_eq!(setup.points_to(), setup.target);
        }
        let leftovers: Vec<_> = fs::read_dir(setup.path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["moil"]);
    }

    /// What the owner may have put where moil would link.
    enum Theirs {
        File,
        Folder,
        Link(&'static str),
    }

    #[test]
    fn leaves_anything_else_alone() {
        let own_build = "/Users/ada/src/moil/target/release/moil";
        let lookalike = "/opt/Moilette.app/moil";
        let cases = [
            (Theirs::File, "a file".to_owned()),
            (Theirs::Folder, "a folder".to_owned()),
            (Theirs::Link(own_build), format!("a link to {own_build}")),
            (Theirs::Link(lookalike), format!("a link to {lookalike}")),
        ];
        for (theirs, what) in cases {
            let setup = Setup::new();
            setup.put(|path| match theirs {
                Theirs::File => fs::write(path, "#!/bin/sh\necho mine\n").unwrap(),
                Theirs::Folder => fs::create_dir(path).unwrap(),
                Theirs::Link(to) => symlink(to, path).unwrap(),
            });
            let before = fs::symlink_metadata(&setup.path).unwrap();
            assert_eq!(setup.link(), Linked::LeftAlone(what.clone()));
            let after = fs::symlink_metadata(&setup.path).unwrap();
            assert_eq!(
                (before.file_type(), before.len(), before.modified().unwrap()),
                (after.file_type(), after.len(), after.modified().unwrap()),
                "{what}"
            );
        }
    }

    #[test]
    fn links_nothing_to_a_bundle_without_moil() {
        let setup = Setup::new();
        fs::remove_file(&setup.target).unwrap();
        let err = link(&setup.path, &setup.target).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!setup.path.parent().unwrap().exists());
    }

    #[test]
    fn only_bundles_that_stay_where_they_are_are_linked_to() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/x1/abc/T/AppTranslocation/0F3C2D1E/d/Moil.app"
        )));
        assert!(!is_translocated(Path::new("/Applications/Moil.app")));
    }
}
