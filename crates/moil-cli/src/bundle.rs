//! Commands for bundle authors: `hash`, `check`, `init` and `run`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Subcommand;
use moil_bundle::{Bundle, PackageSource};
use moil_proto::{FileName, JobEvent, JobId, LogLevel, Outcome, Phase, UrlPolicy};
use moil_runtime::{Executor, Job, Runtime, RuntimeConfig, StopReason, Uv, stop_signal};
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use crate::Globals;

mod storage;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print a bundle's hash.
    Hash {
        /// The bundle's directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Check that a bundle is valid and its lockfile current, and show what
    /// it holds.
    Check {
        /// The bundle's directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Create a bundle to start from: job.py, its lockfile and a manifest.
    Init {
        /// Where to create it.
        dir: PathBuf,
        /// The bundle's name [default: the directory's name]
        #[arg(long)]
        name: Option<String>,
    },
    /// Run a bundle here, exactly as a machine runs a job.
    ///
    /// Inputs are served, and outputs received, by a local HTTP server, so
    /// they move as they would for a real job. Without --home the job runs
    /// in a temporary home, but bundles, their environments and assets stay
    /// cached (in $XDG_CACHE_HOME/moil, ~/Library/Caches/moil on macOS)
    /// between runs.
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// The bundle's directory.
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// The job's params, as JSON or as @FILE to read them from a file.
    #[arg(long, value_name = "JSON|@FILE")]
    params: Option<String>,
    /// An input for the job, served from a local file. Repeatable.
    #[arg(long = "input", value_name = "NAME=PATH", value_parser = parse_input)]
    inputs: Vec<(FileName, PathBuf)>,
    /// An output the job may produce. Repeatable.
    #[arg(long = "output", value_name = "NAME")]
    outputs: Vec<FileName>,
    /// Where to write the outputs.
    #[arg(long, value_name = "DIR", default_value = "out")]
    out_dir: PathBuf,
    /// Stop the job after this many seconds.
    #[arg(long, value_name = "SECS", value_parser = parse_seconds)]
    timeout: Option<Duration>,
    /// Keep the job's working directory, and print where it is.
    #[arg(long)]
    keep: bool,
    /// Print events, then the outcome, as JSON lines.
    #[arg(long)]
    json: bool,
}

pub async fn run(command: Command, globals: &Globals) -> anyhow::Result<()> {
    match command {
        Command::Hash { dir } => {
            println!("{}", load(&dir)?.hash());
            Ok(())
        }
        Command::Check { dir } => check(&dir).await,
        Command::Init { dir, name } => init(&dir, name).await,
        Command::Run(args) => {
            let code = run_bundle(args, globals).await?;
            if code != 0 {
                // Everything the run held, such as a temporary home, has
                // been cleaned up by now.
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

fn load(dir: &Path) -> anyhow::Result<Bundle> {
    Bundle::from_dir(dir).with_context(|| format!("{} doesn't hold a valid bundle", dir.display()))
}

/// uv as bundle authors run it: like a machine does, except that it keeps
/// using the author's `UV_CACHE_DIR`, if they set one.
fn uv() -> anyhow::Result<Uv> {
    let path = moil_runtime::find_uv().context(
        "moil needs uv, which it couldn't find. Install it \
         (https://docs.astral.sh/uv/) or set MOIL_UV to its path",
    )?;
    Ok(Uv::new(path).with_env(uv_cache_env()))
}

fn uv_cache_env() -> Option<(OsString, OsString)> {
    let cache = std::env::var_os("UV_CACHE_DIR").filter(|dir| !dir.is_empty())?;
    Some(("UV_CACHE_DIR".into(), cache))
}

async fn check(dir: &Path) -> anyhow::Result<()> {
    let bundle = load(dir)?;
    let manifest = bundle.manifest();
    println!("{} {}", manifest.name, manifest.version);
    if let Some(description) = &manifest.description {
        println!("{description}");
    }
    println!();
    println!("hash      {}", bundle.hash());
    if bundle.packages().is_empty() {
        println!("packages  none, only the standard library");
    } else {
        println!("packages");
        for package in bundle.packages() {
            let version = package.version.as_deref().unwrap_or("(no version)");
            println!(
                "  {} {version}  {}",
                package.name,
                describe_source(&package.source)
            );
        }
    }
    if !manifest.assets.is_empty() {
        println!("assets");
        for (name, asset) in &manifest.assets {
            let size = asset.size_bytes.map_or_else(
                || "size not given".to_owned(),
                |size| format!("{size} bytes"),
            );
            println!("  {name}  {size}  {}", asset.url);
        }
    }
    if let Some(memory) = manifest.resources.memory_bytes {
        println!("memory    {memory} bytes");
    }
    if let Some(gpu) = manifest.resources.gpu {
        println!(
            "gpu       {}",
            serde_json::to_value(gpu)?.as_str().unwrap_or_default()
        );
    }

    let mut problems = Vec::new();
    if let Err(problem) = check_lockfile(dir).await? {
        problems.push(problem);
    }

    println!();
    if problems.is_empty() {
        println!("ok: the bundle is valid and job.py.lock matches job.py");
        return Ok(());
    }
    for problem in &problems {
        eprintln!("problem: {problem}");
    }
    bail!(
        "the bundle has {} problem{}",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    )
}

fn describe_source(source: &PackageSource) -> String {
    match source {
        PackageSource::Registry(index) => format!("from {index}"),
        PackageSource::Git(url) => format!("from git {url}"),
        PackageSource::Url(url) => format!("from {url}"),
    }
}

/// Whether job.py.lock matches the dependencies job.py declares, as
/// `uv run --locked` will insist on every machine. `Err` explains how not.
async fn check_lockfile(dir: &Path) -> anyhow::Result<Result<(), String>> {
    let output = uv()?
        .command()
        .args(["lock", "--check", "--script", moil_bundle::SCRIPT])
        .current_dir(dir)
        .output()
        .await
        .context("couldn't run uv")?;
    if output.status.success() {
        return Ok(Ok(()));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(Err(if stderr.contains("needs to be updated") {
        format!(
            "job.py.lock doesn't match the dependencies in job.py; run `uv lock --script job.py` in {}",
            dir.display()
        )
    } else {
        format!("uv couldn't check job.py.lock:\n{}", stderr.trim())
    }))
}

const TEMPLATE: &str = r#"# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""__NAME__: a moil job bundle.

The runtime starts this script with the job on stdin and reads events from
its stdout, one JSON object per line (moil spec, section 8). After changing
the dependencies above, run `uv lock --script job.py` to pin them. Try it:

    moil run . --params '{"name": "Ada"}'
"""

import json
import sys


def emit(type, **fields):
    """Sends one event to the runtime: progress, log, data, result or error."""
    print(json.dumps({"v": 1, "type": type, **fields}), flush=True)


def main(job):
    params = job["params"] or {}
    emit("progress", fraction=0.0, message="starting")

    # job["inputs"] maps input names to downloaded files. Write outputs into
    # job["output_dir"], under names from job["outputs"], and list them in
    # the result's "files".
    greeting = f"hello, {params.get('name', 'world')}"
    emit("log", level="info", message=greeting)

    # To fail instead: emit("error", message="...", retryable=False)
    emit("result", files=[], meta={"greeting": greeting})


if __name__ == "__main__":
    main(json.load(sys.stdin))
"#;

async fn init(dir: &Path, name: Option<String>) -> anyhow::Result<()> {
    let name = match name {
        Some(name) => name,
        None => default_name(dir)?,
    };
    if !moil_proto::ids::is_bundle_name(&name) {
        bail!(
            "{name:?} can't be a bundle name: use 1–64 characters of a-z, 0-9 and '-', \
             starting with a letter or digit (pass --name)"
        );
    }
    for file in moil_bundle::FILE_NAMES {
        if dir.join(file).exists() {
            bail!(
                "{} already exists; moil init doesn't overwrite bundles",
                dir.join(file).display()
            );
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    // Written out rather than serialized, to keep the fields in the order
    // people read them. The name has been checked to need no escaping.
    let manifest = format!(
        r#"{{
  "moil": 1,
  "name": "{name}",
  "version": "0.1.0",
  "description": "Says hello."
}}
"#
    );
    std::fs::write(
        dir.join(moil_bundle::SCRIPT),
        TEMPLATE.replace("__NAME__", &name),
    )?;
    std::fs::write(dir.join(moil_bundle::MANIFEST), manifest)?;
    let output = uv()?
        .command()
        .args(["lock", "--script", moil_bundle::SCRIPT])
        .current_dir(dir)
        .output()
        .await
        .context("couldn't run uv")?;
    if !output.status.success() {
        bail!(
            "uv couldn't lock job.py:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let bundle = load(dir)?;
    println!("created bundle {name} in {}", dir.display());
    println!("hash {}", bundle.hash());
    println!();
    println!("next: edit job.py, then `moil check` and `moil run` in that directory");
    Ok(())
}

/// A bundle name made from the name of `dir`.
fn default_name(dir: &Path) -> anyhow::Result<String> {
    let absolute = std::path::absolute(dir)?;
    let base = absolute
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut name = String::new();
    for c in base.chars() {
        match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => name.push(c),
            _ if !name.is_empty() && !name.ends_with('-') => name.push('-'),
            _ => {}
        }
    }
    let name = name.trim_end_matches('-');
    Ok(name
        .chars()
        .take(64)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned())
}

async fn run_bundle(args: RunArgs, globals: &Globals) -> anyhow::Result<i32> {
    let bundle = load(&args.dir)?;
    let params = parse_params(args.params.as_deref())?;
    let mut inputs = BTreeMap::new();
    for (name, path) in args.inputs {
        if !path.is_file() {
            bail!("input {name}: {} isn't a file", path.display());
        }
        if inputs.insert(name.clone(), path).is_some() {
            bail!("input {name} is given twice");
        }
    }
    let outputs: BTreeSet<FileName> = args.outputs.into_iter().collect();
    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("couldn't create {}", args.out_dir.display()))?;

    // A throwaway home unless one is given. Bundles, environments and assets
    // are kept by hash, and bundles and assets verified, so they can be kept
    // between runs all the same: a bundle's environment is then reused, and
    // big assets fetched once.
    let mut temporary = None;
    let home = match &globals.home {
        Some(home) => home.clone(),
        None => {
            let dir = tempfile::Builder::new().prefix("moil-run-").tempdir()?;
            temporary.insert(dir).path().to_owned()
        }
    };
    let mut config = RuntimeConfig::new(uv()?.program().to_owned(), &home);
    if globals.home.is_none()
        && let Some(cache) = cache_dir()
    {
        config.bundles = moil_bundle::BundleStore::new(cache.join("bundles"));
        config.assets_dir = cache.join("assets");
        config.environments_dir = cache.join("environments");
    }
    config.extra_env.extend(uv_cache_env());
    config.keep_work_dirs = args.keep;
    config.echo_stderr = true;
    let runtime = Runtime::new(config)?;
    if globals.home.is_some() {
        runtime.reap_orphans()?;
    }

    let storage = storage::Storage::start(inputs.clone(), outputs.clone(), args.out_dir.clone())
        .await
        .context("couldn't start the local file server")?;
    let job = Job {
        id: JobId::new(format!("local-{}", std::process::id()))?,
        attempt: 1,
        bundle,
        params,
        inputs: inputs
            .keys()
            .map(|name| (name.clone(), storage.download(name)))
            .collect(),
        outputs: outputs
            .iter()
            .map(|name| (name.clone(), storage.upload(name)))
            .collect(),
        timeout: args.timeout,
        // Inputs and outputs move through this command's own loopback server.
        url_policy: UrlPolicy {
            loopback_http: true,
            any_http: false,
        },
    };
    let work_dir = runtime.work_dir(&job.id, job.attempt);

    let printer = Printer { json: args.json };
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let (stopper, stop) = stop_signal();
    let mut interrupts = signal(SignalKind::interrupt())?;
    let attempt = runtime.run(job, events, stop);
    tokio::pin!(attempt);
    let mut stopping = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut attempt => break Some(outcome),
            Some(event) = received.recv() => printer.event(&event),
            _ = interrupts.recv() => {
                if stopping {
                    // Dropping the attempt kills its processes at once.
                    break None;
                }
                stopping = true;
                stopper.stop(StopReason::Interrupted);
                eprintln!("moil: stopping the job; press Ctrl-C again to kill it now");
            }
        }
    };
    while let Ok(event) = received.try_recv() {
        printer.event(&event);
    }
    if args.keep {
        eprintln!("moil: kept the working directory {}", work_dir.display());
        if let Some(home) = temporary.take() {
            let _ = home.keep();
        }
    }
    let Some(outcome) = outcome else {
        return Ok(130);
    };
    printer.outcome(&outcome, &args.out_dir);
    Ok(match &outcome {
        Outcome::Succeeded { .. } => 0,
        _ if stopping => 130,
        _ => 1,
    })
}

/// Where `moil run` keeps bundles and assets between runs without a home:
/// `$XDG_CACHE_HOME/moil`, else `~/Library/Caches/moil` on macOS and
/// `~/.cache/moil` elsewhere.
fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            if cfg!(target_os = "macos") {
                home.join("Library/Caches")
            } else {
                home.join(".cache")
            }
        }
    };
    Some(base.join("moil"))
}

fn parse_params(arg: Option<&str>) -> anyhow::Result<Value> {
    let Some(arg) = arg else {
        return Ok(Value::Null);
    };
    match arg.strip_prefix('@') {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("couldn't read params from {path}"))?;
            serde_json::from_str(&text).with_context(|| format!("{path} isn't valid JSON"))
        }
        None => serde_json::from_str(arg).context("--params isn't valid JSON"),
    }
}

fn parse_input(arg: &str) -> Result<(FileName, PathBuf), String> {
    let (name, path) = arg
        .split_once('=')
        .ok_or_else(|| format!("{arg:?} isn't NAME=PATH"))?;
    let name = FileName::new(name).map_err(|e| e.to_string())?;
    Ok((name, PathBuf::from(path)))
}

fn parse_seconds(arg: &str) -> Result<Duration, String> {
    arg.parse::<f64>()
        .ok()
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .filter(|limit| !limit.is_zero())
        .ok_or_else(|| format!("{arg:?} isn't a number of seconds"))
}

/// Shows a job's events and outcome, for people or, with `json`, as JSON
/// lines on stdout. Diagnostics go to stderr, along with the script's own
/// stderr.
struct Printer {
    json: bool,
}

impl Printer {
    fn event(&self, event: &JobEvent) {
        if self.json {
            /// An event as the wire has it, `"v": 1` first.
            #[derive(serde::Serialize)]
            struct Line<'a>(#[serde(with = "moil_proto::event::versioned")] &'a JobEvent);
            println!(
                "{}",
                serde_json::to_string(&Line(event)).expect("events serialize")
            );
            return;
        }
        match event {
            JobEvent::Phase { phase } => println!("{}", phase_name(*phase)),
            JobEvent::Progress { fraction, message } => {
                let percent = fraction.map_or_else(String::new, |f| format!("{:.0}%", f * 100.0));
                println!("  {percent:<6} {}", message.as_deref().unwrap_or_default());
            }
            JobEvent::Log { level, message } => println!("  {:<6} {message}", level_name(*level)),
            JobEvent::Data { payload } => println!("  data   {payload}"),
        }
    }

    fn outcome(&self, outcome: &Outcome, out_dir: &Path) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string(outcome).expect("outcomes serialize")
            );
            return;
        }
        match outcome {
            Outcome::Succeeded { result } => {
                println!("succeeded");
                if let Some(meta) = &result.meta {
                    println!("  meta   {meta}");
                }
                for (name, file) in &result.files {
                    println!(
                        "  {}  {} bytes  sha256 {}",
                        out_dir.join(name.as_str()).display(),
                        file.size_bytes,
                        file.sha256
                    );
                }
            }
            Outcome::Failed { error } => {
                println!("failed: {}: {}", error.code, error.message);
                if let Some(code) = error.exit_code {
                    println!("  exit status {code}");
                }
                if error.retryable {
                    println!("  another attempt could succeed");
                }
                if error.code == moil_proto::ErrorCode::OutputMissing {
                    println!("  declare outputs with --output NAME");
                }
            }
            Outcome::Cancelled => println!("cancelled"),
        }
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Preparing => "preparing",
        Phase::Downloading => "downloading",
        Phase::Running => "running",
        Phase::Uploading => "uploading",
    }
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}
