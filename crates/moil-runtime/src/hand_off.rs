//! Leaving a succeeded attempt's outputs to the machine's owner (spec §8.5).

use std::io;
use std::path::{Path, PathBuf};

use moil_proto::FileName;

use crate::workdir::open_output;

/// Moves the outputs `names` from `out_dir` into `to`, or copies them if
/// `copy` or if they can't be renamed there. Either every output ends up in
/// `to`, or none does and the error says why, as in "couldn't move output
/// a.txt to …: …".
pub(crate) fn hand_off<'a>(
    out_dir: &Path,
    names: impl IntoIterator<Item = &'a FileName>,
    to: &Path,
    copy: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(to)
        .map_err(|e| format!("couldn't create {} for the outputs: {e}", to.display()))?;
    let mut placed: Vec<PathBuf> = Vec::new();
    for name in names {
        let target = to.join(name.as_str());
        if let Err(why) = place(out_dir, name, &target, copy) {
            for path in &placed {
                let _ = std::fs::remove_file(path);
            }
            return Err(format!(
                "couldn't move output {name} to {}: {why}",
                to.display()
            ));
        }
        placed.push(target);
    }
    Ok(())
}

/// Puts output `name` at `target`: renames it there unless `copy` or the
/// rename would cross file systems, and copies it otherwise.
fn place(out_dir: &Path, name: &FileName, target: &Path, copy: bool) -> Result<(), String> {
    if !copy {
        match std::fs::rename(out_dir.join(name.as_str()), target) {
            Ok(()) => return still_a_file(name, target),
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    // As when uploading: a regular file right in out/, through no link.
    let mut source = open_output(out_dir, name)?;
    let copied =
        std::fs::File::create(target).and_then(|mut file| io::copy(&mut source, &mut file));
    if let Err(e) = copied {
        let _ = std::fs::remove_file(target);
        return Err(e.to_string());
    }
    Ok(())
}

/// Checks that what a rename put at `target` is still the regular file
/// that was uploaded, not something a process that outlived the script put
/// in its place, such as a link to one of the owner's files.
fn still_a_file(name: &FileName, target: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        _ => {
            let _ = std::fs::remove_file(target);
            let _ = std::fs::remove_dir_all(target);
            Err(format!("out/{name} is no longer a regular file"))
        }
    }
}
