//! Running uv the way the spec asks (§8.1): with its own configuration
//! only, with uv-managed Python, and with a minimal environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use moil_proto::{ErrorCode, JobError};
use tokio::process::Command;

use crate::stderr::{self, Tail};

/// The uv binary, and the environment every uv process gets.
#[derive(Debug, Clone)]
pub struct Uv {
    program: PathBuf,
    extra_env: Vec<(OsString, OsString)>,
}

/// Variables that pin uv's behavior, whatever the machine's own setup.
const UV_ENV: [(&str, &str); 3] = [
    // Ignore uv.toml and pyproject.toml files around the bundle and in
    // the owner's home.
    ("UV_NO_CONFIG", "1"),
    // Never run on the machine's own Python installs.
    ("UV_MANAGED_PYTHON", "1"),
    // Trust the roots the platform trusts, like the runtime does, so a
    // network with its own CA works for packages too.
    ("UV_SYSTEM_CERTS", "1"),
];

/// Used when the parent has no `PATH`.
const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

impl Uv {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            extra_env: Vec::new(),
        }
    }

    /// Adds variables to the environment of every uv process, after (and
    /// so over) the ones the runtime sets.
    pub fn with_env(mut self, vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.extra_env.extend(vars);
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// A command running uv with a minimal environment: this process's
    /// `PATH`, `HOME`, `LANG` and `TMPDIR`, the variables that pin uv's
    /// behavior, and the extra variables. Nothing else from this process's
    /// environment gets through.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.env_clear();
        command.env("PATH", env_or("PATH", DEFAULT_PATH));
        for name in ["HOME", "TMPDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command.env("LANG", env_or("LANG", "C.UTF-8"));
        command.envs(UV_ENV);
        command.envs(self.extra_env.iter().map(|(k, v)| (k, v)));
        command
    }
}

fn env_or(name: &str, default: &str) -> OsString {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.into())
}

/// Creates the environment for the script at `script` with
/// `uv sync --locked --script`, failing with `environment` if uv can't.
pub(crate) async fn sync(uv: &Uv, script: &Path, echo_stderr: bool) -> Result<(), JobError> {
    let mut command = uv.command();
    command
        .args(["sync", "--locked", "--script"])
        .arg(script)
        .current_dir(script.parent().unwrap_or(Path::new("/")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| cant_start(uv, &error))?;
    let tail = Arc::new(Mutex::new(Tail::default()));
    let stderr = child.stderr.take().expect("stderr is piped");
    let (status, ()) = tokio::join!(
        child.wait(),
        stderr::collect(stderr, echo_stderr, tail.clone())
    );
    let tail = tail.lock().unwrap_or_else(|e| e.into_inner()).render();
    let status = status.map_err(|error| cant_start(uv, &error))?;
    if status.success() {
        return Ok(());
    }
    let mut error = sync_failure(&tail);
    if let Some(code) = status.code() {
        error = error.with_exit_code(code);
    }
    Err(error.with_stderr_tail(tail))
}

/// A failure to run uv at all: the machine's problem, not the bundle's, so
/// another machine may well succeed.
pub(crate) fn cant_start(uv: &Uv, error: &std::io::Error) -> JobError {
    JobError::new(
        ErrorCode::Internal,
        format!("couldn't run uv ({}): {error}", uv.program.display()),
    )
}

/// Explains a failed `uv sync` from its stderr. Retryable, as an index
/// that is down may be back for the next attempt, unless the bundle itself
/// is at fault (spec §9).
fn sync_failure(stderr: &str) -> JobError {
    if stderr.contains("needs to be updated") {
        return JobError::new(
            ErrorCode::Environment,
            "job.py.lock doesn't match the dependencies job.py declares; \
             the bundle's author must run `uv lock --script job.py`",
        )
        .retryable(false);
    }
    let message = match stderr
        .lines()
        .find_map(|line| line.trim().strip_prefix("error:"))
    {
        Some(reason) => format!("uv couldn't create the environment: {}", reason.trim()),
        None => "uv couldn't create the environment".into(),
    };
    JobError::new(ErrorCode::Environment, message)
}

/// Where uv is: `explicit` (`$MOIL_UV`) if given, else a `uv` next to
/// `exe`, else the first `uv` on `path`.
pub(crate) fn locate(
    explicit: Option<OsString>,
    exe: Option<PathBuf>,
    path: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit.filter(|p| !p.is_empty()) {
        return Some(explicit.into());
    }
    let beside_exe = exe
        .as_deref()
        .and_then(Path::parent)
        .map(|dir| dir.join("uv"));
    let on_path = path
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .map(|dir| dir.join("uv"));
    beside_exe
        .into_iter()
        .chain(on_path)
        .find(|candidate| is_executable(candidate))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_uv(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("uv");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn uv_is_found_by_env_then_beside_the_executable_then_on_path() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("App.app/Contents/MacOS");
        let bin = root.path().join("bin");
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let bundled = fake_uv(&app);
        let installed = fake_uv(&bin);
        let exe = Some(app.join("moil"));
        let path = Some(std::env::join_paths([&empty, &bin]).unwrap());

        assert_eq!(
            locate(Some("/opt/uv".into()), exe.clone(), path.clone()),
            Some("/opt/uv".into())
        );
        assert_eq!(locate(None, exe.clone(), path.clone()), Some(bundled));
        assert_eq!(
            locate(Some("".into()), None, path.clone()),
            Some(installed.clone())
        );
        assert_eq!(
            locate(None, Some(empty.join("moil")), path),
            Some(installed)
        );
        assert_eq!(locate(None, exe.map(|_| empty.join("moil")), None), None);
    }

    #[test]
    fn a_file_without_execute_permission_is_not_uv() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("uv"), "").unwrap();
        assert_eq!(locate(None, None, Some(root.path().into())), None);
    }
}
