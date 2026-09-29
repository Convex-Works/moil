//! Leaving a succeeded attempt's outputs to the machine's owner (spec §8.5).

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::Path;

use moil_proto::FileName;

use crate::sys::{self, FileId};

/// Moves the outputs `uploaded` names from `out_dir` into `to`, or copies
/// them if `copy` or if they can't be renamed there. Either every output
/// ends up in `to`, or none does and the error says why, as in "couldn't
/// move output a.txt to …: …".
///
/// What ends up in `to` is exactly the files that were uploaded, which
/// `uploaded` names by [`FileId`], even if a process that outlived the
/// script changed `out/` since: `out/` is used only if it is still a
/// directory rather than a symbolic link, everything is done in the
/// directory it was then, and an output that is no longer the file that
/// was uploaded is left where it was.
pub(crate) fn hand_off(
    out_dir: &Path,
    uploaded: &BTreeMap<FileName, FileId>,
    to: &Path,
    copy: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(to)
        .map_err(|e| format!("couldn't create {} for the outputs: {e}", to.display()))?;
    let into = sys::open_dir(to)
        .map_err(|e| format!("couldn't open {} for the outputs: {e}", to.display()))?;
    let from = sys::open_dir(out_dir).map_err(|e| match e.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => "out/ is no longer a directory".to_owned(),
        _ => format!("out/ couldn't be opened: {e}"),
    })?;
    let mut placed: Vec<&FileName> = Vec::new();
    for (name, id) in uploaded {
        if let Err(why) = place(&from, name, *id, &into, copy) {
            // Every one placed is the uploaded file, or a copy of it.
            for name in placed {
                let _ = sys::unlink_at(&into, name.as_str());
            }
            return Err(format!(
                "couldn't move output {name} to {}: {why}",
                to.display()
            ));
        }
        placed.push(name);
    }
    Ok(())
}

/// Puts output `name`, the file `id` names, from `from` into `into`:
/// renames it there unless `copy` or the rename would cross file systems,
/// and copies it otherwise.
fn place(from: &File, name: &FileName, id: FileId, into: &File, copy: bool) -> Result<(), String> {
    let name = name.as_str();
    let swapped = || format!("out/{name} is no longer the file that was uploaded");
    if !copy {
        if FileId::at(from, name).map_err(|e| e.to_string())? != id {
            return Err(swapped());
        }
        match sys::rename_at(from, name, into, name) {
            // It may have been swapped since it was checked: then it goes
            // back where it was.
            Ok(()) if FileId::at(into, name).is_ok_and(|moved| moved == id) => return Ok(()),
            Ok(()) => {
                let _ = sys::rename_at(into, name, from, name);
                return Err(swapped());
            }
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut source = sys::open_at(from, name).map_err(|e| e.to_string())?;
    if FileId::of(&source).map_err(|e| e.to_string())? != id {
        return Err(swapped());
    }
    let mut target = sys::create_at(into, name).map_err(|e| e.to_string())?;
    if let Err(e) = io::copy(&mut source, &mut target) {
        let _ = sys::unlink_at(into, name);
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// An attempt's `out/` holding `a.txt`, uploaded, and a folder of the
    /// owner's holding a file of the same name.
    struct Outputs {
        root: tempfile::TempDir,
        uploaded: BTreeMap<FileName, FileId>,
    }

    impl Outputs {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let out = root.path().join("out");
            std::fs::create_dir(&out).unwrap();
            std::fs::write(out.join("a.txt"), "the output").unwrap();
            let owners = root.path().join("owners");
            std::fs::create_dir(&owners).unwrap();
            std::fs::write(owners.join("a.txt"), "the owner's").unwrap();
            let file = File::open(out.join("a.txt")).unwrap();
            let uploaded = BTreeMap::from([("a.txt".parse().unwrap(), FileId::of(&file).unwrap())]);
            Self { root, uploaded }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }

        fn hand_off(&self, copy: bool) -> Result<(), String> {
            hand_off(&self.path("out"), &self.uploaded, &self.path("kept"), copy)
        }

        fn kept(&self) -> Vec<String> {
            std::fs::read_dir(self.path("kept"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        }

        fn owners_file(&self) -> String {
            std::fs::read_to_string(self.path("owners/a.txt")).unwrap()
        }
    }

    #[test]
    fn the_uploaded_outputs_are_moved_or_copied() {
        for copy in [false, true] {
            let outputs = Outputs::new();
            outputs.hand_off(copy).unwrap();
            assert_eq!(
                std::fs::read_to_string(outputs.path("kept/a.txt")).unwrap(),
                "the output"
            );
            assert_eq!(outputs.path("out/a.txt").exists(), copy);
        }
    }

    /// A process that outlived the script makes `out/` a link to a folder
    /// of the owner's, once the outputs are up.
    #[test]
    fn out_swapped_for_a_link_hands_off_nothing_of_the_owners() {
        for copy in [false, true] {
            let outputs = Outputs::new();
            std::fs::rename(outputs.path("out"), outputs.path("out.real")).unwrap();
            std::os::unix::fs::symlink(outputs.path("owners"), outputs.path("out")).unwrap();

            let why = outputs.hand_off(copy).unwrap_err();
            assert!(why.ends_with("out/ is no longer a directory"), "{why}");
            assert_eq!(outputs.owners_file(), "the owner's");
            assert_eq!(outputs.kept(), [] as [String; 0]);
        }
    }

    /// ...or puts one of the owner's files, linked, where an output was.
    #[test]
    fn an_output_swapped_for_another_file_is_left_where_it_is() {
        for copy in [false, true] {
            let outputs = Outputs::new();
            std::fs::remove_file(outputs.path("out/a.txt")).unwrap();
            std::fs::hard_link(outputs.path("owners/a.txt"), outputs.path("out/a.txt")).unwrap();

            let why = outputs.hand_off(copy).unwrap_err();
            assert!(
                why.ends_with("out/a.txt is no longer the file that was uploaded"),
                "{why}"
            );
            assert_eq!(outputs.kept(), [] as [String; 0]);
            assert_eq!(outputs.owners_file(), "the owner's");
            assert!(outputs.path("out/a.txt").exists());
        }
    }
}
