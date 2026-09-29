//! The agent's log: on stderr, and in a file the owner can find, since an
//! app opened from Finder has nowhere to show stderr. The app and `moil
//! agent` both keep it at [`Home::log_file`](crate::Home::log_file)
//! (spec/machine.md §1).

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// How big the log grows before it starts over. The previous one is kept
/// beside it, so the log takes at most twice this.
const MAX_BYTES: u64 = 4 << 20;

/// Logs to stderr and to the file at `path`, at the level `RUST_LOG` asks
/// for (`info` by default), and logs panics as errors. Returns the log
/// file, unless there is none; why not is logged.
pub fn init(path: Option<&Path>) -> Option<PathBuf> {
    let opened = path.map(|path| {
        path.parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| RotatingFile::open(path, MAX_BYTES))
            .map(|file| (path.to_owned(), file))
    });
    let (opened_path, file, error) = match opened {
        Some(Ok((path, file))) => (Some(path), Some(file), None),
        Some(Err(e)) => (None, None, Some(e)),
        None => (None, None, None),
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let stderr = fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal());
    let file = file.map(|file| fmt::layer().with_writer(Mutex::new(file)).with_ansi(false));
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file)
        .init();
    log_panics();

    match (&opened_path, error, path) {
        (Some(path), _, _) => tracing::debug!("logging to {}", path.display()),
        (None, Some(e), Some(path)) => {
            tracing::warn!("couldn't keep a log at {}: {e}", path.display());
        }
        _ => tracing::warn!("couldn't find where to keep a log; logging to stderr only"),
    }
    opened_path
}

/// Sends panics to the log instead of only to stderr, with a backtrace
/// when `RUST_BACKTRACE` asks for one.
fn log_panics() {
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let backtrace = Backtrace::capture();
        match backtrace.status() {
            BacktraceStatus::Captured => {
                tracing::error!("thread '{thread}' {info}\n{backtrace}");
            }
            _ => tracing::error!("thread '{thread}' {info}"),
        }
    }));
}

/// A log file that starts over once it would grow past `max_bytes`,
/// keeping the one before as `<name>.1`. Each write lands whole in one
/// file, so a log line (which the log writes at once) is never split. If
/// someone removes the file, the next write starts a new one.
#[derive(Debug)]
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    file: File,
    len: u64,
}

impl RotatingFile {
    /// Opens the log at `path`, adding to what is there.
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64) -> io::Result<Self> {
        let path = path.into();
        let file = append(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path,
            max_bytes,
            file,
            len,
        })
    }

    /// Where the log before this one is kept.
    pub fn previous(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_owned();
        name.push(".1");
        self.path.with_file_name(name)
    }

    fn reopen(&mut self) -> io::Result<()> {
        self.file = append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

fn append(path: &Path) -> io::Result<File> {
    File::options().create(true).append(true).open(path)
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.path.exists() {
            self.reopen()?;
        } else if self.len > 0 && self.len + buf.len() as u64 > self.max_bytes {
            fs::rename(&self.path, self.previous())?;
            self.reopen()?;
        }
        let written = self.file.write(buf)?;
        self.len += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    /// Writes `line n` for each `n`, each as one write, as the log does.
    fn write_lines(log: &mut RotatingFile, lines: std::ops::Range<u32>) {
        for n in lines {
            log.write_all(format!("line {n:03} {}\n", "x".repeat(20)).as_bytes())
                .unwrap();
        }
    }

    #[test]
    fn the_log_starts_over_past_its_limit_keeping_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moil.log");
        let mut log = RotatingFile::open(&path, 100).unwrap();
        write_lines(&mut log, 0..50);

        let (current, previous) = (read(&path), read(&log.previous()));
        assert_eq!(log.previous(), dir.path().join("moil.log.1"));
        assert!(current.len() <= 100 && previous.len() <= 100);
        let lines: Vec<&str> = previous.lines().chain(current.lines()).collect();
        let numbers: Vec<u32> = lines
            .iter()
            .map(|line| line[5..8].parse().unwrap())
            .collect();
        assert_eq!(numbers.last(), Some(&49), "the newest lines are kept");
        assert_eq!(
            numbers,
            (numbers[0]..50).collect::<Vec<_>>(),
            "in order, none missing between the files"
        );
        assert!(lines.iter().all(|line| line.len() == 29), "none split");
        let names = fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(names, 2, "only ever two files");
    }

    #[test]
    fn a_restart_adds_to_the_log_and_counts_what_was_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moil.log");
        RotatingFile::open(&path, 10)
            .unwrap()
            .write_all(b"one\ntwo\n")
            .unwrap();
        let mut log = RotatingFile::open(&path, 10).unwrap();
        log.write_all(b"z\n").unwrap();
        assert_eq!(read(&path), "one\ntwo\nz\n");

        log.write_all(b"three\n").unwrap();
        assert_eq!(read(&log.previous()), "one\ntwo\nz\n");
        assert_eq!(read(&path), "three\n");
    }

    #[test]
    fn a_line_longer_than_the_limit_gets_a_file_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moil.log");
        let mut log = RotatingFile::open(&path, 10).unwrap();
        log.write_all(b"short\n").unwrap();
        let long = format!("{}\n", "y".repeat(30));
        log.write_all(long.as_bytes()).unwrap();
        assert_eq!(read(&path), long);
        log.write_all(b"after\n").unwrap();
        assert_eq!(read(&path), "after\n");
        assert_eq!(read(&log.previous()), long);
    }

    #[test]
    fn a_removed_log_is_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moil.log");
        let mut log = RotatingFile::open(&path, 100).unwrap();
        log.write_all(b"one\n").unwrap();
        fs::remove_file(&path).unwrap();
        log.write_all(b"two\n").unwrap();
        assert_eq!(read(&path), "two\n");
    }
}
