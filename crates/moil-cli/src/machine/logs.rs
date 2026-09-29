//! `moil logs`: printing the agent's log or a hook's, and following it
//! across rotation.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use moil_agent::display::reveal;
use moil_bundle::{OpenError, open_regular};

/// How often a follower looks for more.
const POLL: Duration = Duration::from_millis(100);

/// How many lines `-f` prints before following, without `-n`.
const FOLLOW_LINES: usize = 10;

/// Prints the log at `path`, or its last `lines` lines, then with `follow`
/// what is added to it until interrupted, whatever file is at `path` by
/// then: logs are renamed aside and started afresh once they grow too big.
/// Reads only a regular file, not through a symbolic link: a commit could
/// put anything at a hook's log's path.
pub fn print(path: &Path, follow: bool, lines: Option<usize>) -> anyhow::Result<()> {
    let lines = lines.or(follow.then_some(FOLLOW_LINES));
    let mut out = Lines::default();
    let printed = match open(path) {
        Ok(mut file) => {
            let mut text = Vec::new();
            file.read_to_end(&mut text)
                .with_context(|| format!("couldn't read {}", path.display()))?;
            out.write(last_lines(&text, lines)).map(|()| Some(file))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            match follow {
                true => eprintln!(
                    "There is no log at {} yet; waiting for one.",
                    path.display()
                ),
                false => eprintln!("There is no log at {} yet.", path.display()),
            }
            Ok(None)
        }
        Err(e) => return Err(e).with_context(|| format!("couldn't read {}", path.display())),
    };
    let result = printed.and_then(|file| match follow {
        true => follow_log(path, file, &mut out),
        false => out.finish(),
    });
    match result {
        // Whoever reads what the command prints has stopped reading.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other.with_context(|| format!("couldn't follow {}", path.display()))?),
    }
}

/// The last `lines` lines of `text`, or all of it.
fn last_lines(text: &[u8], lines: Option<usize>) -> &[u8] {
    let Some(lines) = lines else {
        return text;
    };
    if lines == 0 {
        return &[];
    }
    let body = text.strip_suffix(b"\n").unwrap_or(text);
    let start = body
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(lines - 1)
        .map_or(0, |(i, _)| i + 1);
    &text[start..]
}

/// Prints what is added to the log at `path` until interrupted. `file` is
/// the log as it was opened, if it was there, already read to its end.
fn follow_log(path: &Path, mut file: Option<File>, out: &mut Lines) -> io::Result<()> {
    let mut buffer = vec![0; 64 << 10];
    loop {
        if let Some(open) = &mut file {
            let read = open.read(&mut buffer)?;
            if read > 0 {
                out.write(&buffer[..read])?;
                continue;
            }
        }
        // At the end of what is open: is another file at the path by now?
        if let Some(now) = metadata(path)? {
            let Some(open) = &mut file else {
                file = open_if_there(path)?;
                continue;
            };
            let was = open.metadata()?;
            if (was.dev(), was.ino()) != (now.dev(), now.ino()) {
                // What was written before the log was renamed aside, then
                // the new log from its start.
                let mut rest = Vec::new();
                open.read_to_end(&mut rest)?;
                out.write(&rest)?;
                out.finish()?;
                file = open_if_there(path)?;
                continue;
            }
            if now.len() < open.stream_position()? {
                // Emptied in place: start over.
                out.finish()?;
                open.seek(SeekFrom::Start(0))?;
                continue;
            }
        }
        std::thread::sleep(POLL);
    }
}

/// What is at `path`, unless nothing is, as while a log is renamed aside.
fn metadata(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn open_if_there(path: &Path) -> io::Result<Option<File>> {
    match open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Opens the log at `path` to read it, if it is a regular file.
fn open(path: &Path) -> io::Result<File> {
    open_regular(path, File::options().read(true)).map_err(|error| match error {
        OpenError::Io(e) => e,
        refused => io::Error::other(format!(
            "{refused}, and moil reads a log only from a regular file"
        )),
    })
}

/// Prints whole lines as they come in, with hidden characters revealed:
/// logs quote what services say.
#[derive(Debug, Default)]
struct Lines {
    /// The start of a line whose end hasn't come in yet.
    partial: Vec<u8>,
}

impl Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.partial.extend_from_slice(bytes);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Ok(());
        };
        let whole: Vec<u8> = self.partial.drain(..=end).collect();
        let mut stdout = io::stdout().lock();
        stdout.write_all(reveal(&String::from_utf8_lossy(&whole)).as_bytes())?;
        stdout.flush()
    }

    /// Prints the last line, if it has no line feed.
    fn finish(&mut self) -> io::Result<()> {
        if self.partial.is_empty() {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", reveal(&line))?;
        stdout.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_lines_are_whole_lines() {
        let text = b"one\ntwo\nthree\n";
        assert_eq!(last_lines(text, Some(2)), b"two\nthree\n");
        assert_eq!(last_lines(text, Some(5)), text);
        assert_eq!(last_lines(text, Some(0)), b"");
        assert_eq!(last_lines(text, None), text);
        assert_eq!(last_lines(b"one\ntwo", Some(1)), b"two");
    }
}
