//! Commands for machine owners.
//!
//! What scripts and the end-to-end tests read goes to stdout in stable
//! `Label: value` lines (`Code: WDJB-MJHT`, `Paired: klisi.example.com`)
//! or columns; everything else, including prompts, goes to stderr.

use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::process::{Command as Process, Stdio};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Subcommand;
use moil_agent::review::reveal;
use moil_agent::{
    Agent, Home, Machine, MachineConfig, PairedService, Review, Snapshot, default_secrets,
};
use moil_bundle::PackageSource;
use moil_proto::BundleHash;
use moil_runtime::Runtime;

use crate::Globals;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Pair this machine with a service: confirm the code it shows in the
    /// service's web page.
    Pair {
        /// The service's moil URL, e.g. https://klisi.example.com/moil.
        url: String,
        /// What the service should call this machine (remembered for every
        /// service). Defaults to the computer's name.
        #[arg(long)]
        name: Option<String>,
        /// Don't open the confirmation page in a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// List the paired services.
    Services,
    /// Forget a service, and ask it to forget this machine.
    Unpair {
        /// The service's ID, host, URL or name.
        service: String,
    },
    /// List a service's bundles, or show one in full.
    Review {
        /// The service's ID, host, URL or name.
        service: String,
        /// The bundle to show, by its full hash.
        hash: Option<BundleHash>,
    },
    /// Show a bundle in full, then approve it for a service.
    Approve {
        /// The service's ID, host, URL or name.
        service: String,
        /// The bundle to approve, by its full hash.
        hash: BundleHash,
        /// Approve without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Withdraw a bundle's approval.
    Revoke {
        /// The service's ID, host, URL or name.
        service: String,
        /// The approved bundle, by its full hash.
        hash: BundleHash,
    },
    /// Take jobs from every paired service until interrupted.
    Agent,
}

pub async fn run(command: Command, globals: &Globals) -> anyhow::Result<()> {
    let machine = open(globals)?;
    match command {
        Command::Pair {
            url,
            name,
            no_browser,
        } => pair(&machine, &url, name, no_browser).await,
        Command::Services => services(&machine),
        Command::Unpair { service } => {
            let service = machine.find_service(&service)?;
            machine.unpair(&service.id).await?;
            println!("Unpaired: {}", service.id);
            Ok(())
        }
        Command::Review {
            service,
            hash: None,
        } => bundles(&machine, &machine.find_service(&service)?).await,
        Command::Review {
            service,
            hash: Some(hash),
        } => {
            let service = machine.find_service(&service)?;
            show(&machine.review(&service.id, hash).await?);
            Ok(())
        }
        Command::Approve { service, hash, yes } => {
            let service = machine.find_service(&service)?;
            approve(&machine, &machine.review(&service.id, hash).await?, yes)
        }
        Command::Revoke { service, hash } => {
            let service = machine.find_service(&service)?;
            if !service.is_approved(&hash) {
                bail!("{hash} isn't approved for {}", service.id);
            }
            machine.revoke(&service.id, hash)?;
            println!("Revoked: {hash}");
            Ok(())
        }
        Command::Agent => agent(machine).await,
    }
}

fn open(globals: &Globals) -> anyhow::Result<Machine> {
    let root = match &globals.home {
        Some(root) => root.clone(),
        None => Home::default_root()?,
    };
    let home = Home::new(root);
    let secrets = default_secrets(&home)?;
    Ok(Machine::open(MachineConfig {
        home,
        secrets,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    })?)
}

async fn pair(
    machine: &Machine,
    url: &str,
    name: Option<String>,
    no_browser: bool,
) -> anyhow::Result<()> {
    if let Some(name) = name {
        machine.update_settings(|s| s.machine_name = Some(name))?;
    }
    let session = machine.pair(url).await?;
    eprintln!(
        "Pairing with {} at {} as {:?}.",
        session.service_name,
        session.base_url(),
        machine.info().name
    );
    if session.base_url().scheme() == "http" {
        eprintln!("Warning: this service is reached over plain http.");
    }
    println!("Code: {}", session.user_code);
    println!("URL: {}", session.verification_uri_complete);
    if !no_browser && open_browser(&session.verification_uri_complete).is_err() {
        eprintln!("Couldn't open a browser; open the URL above yourself.");
    }
    eprintln!(
        "Check that {} shows the same code, then confirm it there. Waiting…",
        session.service_name
    );
    let service = session.wait().await?;
    println!("Paired: {}", service.id);
    eprintln!(
        "{} can now offer this machine jobs. See its bundles with `moil review {}`.",
        service.name, service.id
    );
    Ok(())
}

fn open_browser(url: &str) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        Process::new("open")
    } else if cfg!(windows) {
        let mut command = Process::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        Process::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

fn services(machine: &Machine) -> anyhow::Result<()> {
    let services = machine.services()?;
    if services.is_empty() {
        eprintln!("No paired services. Pair one with `moil pair URL`.");
        return Ok(());
    }
    let rows: Vec<[String; 4]> = services
        .iter()
        .map(|s| {
            [
                s.id.to_string(),
                s.base_url.to_string(),
                s.approvals.len().to_string(),
                s.name.clone(),
            ]
        })
        .collect();
    table(&["ID", "URL", "APPROVED", "NAME"], &rows);
    Ok(())
}

/// The service's bundles and whether each is approved, plus approved
/// bundles it no longer offers.
async fn bundles(machine: &Machine, service: &PairedService) -> anyhow::Result<()> {
    let offered = machine.bundles(&service.id).await?;
    let listed: HashSet<BundleHash> = offered.iter().map(|b| b.hash).collect();
    let mut rows: Vec<[String; 3]> = offered
        .iter()
        .map(|b| {
            let status = if service.is_approved(&b.hash) {
                "approved"
            } else {
                "not approved"
            };
            [
                b.hash.to_string(),
                status.into(),
                format!("{} {}", b.name, b.version),
            ]
        })
        .collect();
    rows.extend(
        service
            .approvals
            .iter()
            .filter(|a| !listed.contains(&a.hash))
            .map(|a| {
                [
                    a.hash.to_string(),
                    "approved, no longer offered".into(),
                    format!("{} {}", a.name, a.version),
                ]
            }),
    );
    if rows.is_empty() {
        eprintln!("{} offers no bundles.", service.name);
        return Ok(());
    }
    table(&["HASH", "STATUS", "BUNDLE"], &rows);
    eprintln!("Show one in full with `moil review {} HASH`.", service.id);
    Ok(())
}

/// Everything the owner needs to decide (spec §5.5): the service, the three
/// files in full, the packages, the hash and how to check it, and what
/// changed since the version approved before.
fn show(review: &Review) {
    let (bundle, service) = (&review.bundle, &review.service);
    let manifest = bundle.manifest();
    println!("Service: {} ({})", service.name, service.base_url);
    if service.is_insecure() {
        println!("Warning: this service is reached over plain http.");
    }
    println!("Bundle: {} {}", manifest.name, manifest.version);
    if let Some(description) = &manifest.description {
        println!("Description: {}", reveal(description));
    }
    println!("Hash: {}", bundle.hash());
    let status = if review.approved {
        "approved"
    } else {
        "not approved"
    };
    println!("Status: {status}");
    if let Some(memory) = manifest.resources.memory_bytes {
        println!("Memory: {}", size(memory));
    }
    if let Some(gpu) = manifest.resources.gpu {
        println!("GPU: {}", format!("{gpu:?}").to_lowercase());
    }
    for (name, asset) in &manifest.assets {
        let size = asset
            .size_bytes
            .map(size)
            .unwrap_or_else(|| "size unknown".into());
        println!(
            "Asset: {name} ({size}) from {}, sha256 {}",
            asset.url, asset.sha256
        );
    }

    println!();
    println!("To check the hash yourself, save the three files below into one directory and run");
    println!("  {}", Review::VERIFY_COMMAND);
    println!("It must print {}.", bundle.hash());
    for (name, text) in bundle.files() {
        println!();
        println!("==> {name} <==");
        print!("{}", reveal(text));
        if !text.ends_with('\n') {
            println!();
            println!("(no newline at the end of {name})");
        }
    }

    println!();
    let packages = bundle.packages();
    println!("Packages ({}):", packages.len());
    for package in packages {
        let version = package.version.as_deref().unwrap_or("(no version)");
        let source = match (&package.source, package.link()) {
            (_, Some(link)) => link,
            (PackageSource::Path(path), None) => format!("a path on the author's machine: {path}"),
            (PackageSource::Registry(index), None) => format!("from {index}"),
            (source, None) => format!("{source:?}"),
        };
        println!("  {} {version}  {}", package.name, reveal(&source));
    }

    println!();
    match (&review.previous, review.diff()) {
        (Some(previous), Some(diffs)) => {
            println!(
                "Changes since {} {} ({}):",
                previous.manifest().name,
                previous.manifest().version,
                previous.hash()
            );
            for diff in diffs {
                if diff.is_unchanged() {
                    println!("  {} is unchanged", diff.name);
                } else {
                    print!("{}", reveal(&diff.unified()));
                }
            }
        }
        _ => println!(
            "No earlier version of {} is approved for {}.",
            manifest.name, service.name
        ),
    }
    println!();
    println!(
        "Approving lets this code run on this machine with your permissions whenever {} \
         sends it a job. moil has no sandbox yet.",
        service.name
    );
}

fn approve(machine: &Machine, review: &Review, yes: bool) -> anyhow::Result<()> {
    show(review);
    let hash = review.bundle.hash();
    if review.approved {
        println!("Approved: {hash}");
        eprintln!("It was already approved.");
        return Ok(());
    }
    if !yes {
        let manifest = review.bundle.manifest();
        eprint!(
            "Approve {} {} for {}? [y/N] ",
            manifest.name, manifest.version, review.service.name
        );
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !std::io::stdin().is_terminal() {
            eprintln!("{}", answer.trim());
        }
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            bail!("{hash} was not approved");
        }
    }
    machine.approve(review)?;
    println!("Approved: {hash}");
    Ok(())
}

/// Runs the agent with the real runtime until SIGINT or SIGTERM, then
/// shuts it down: the running job is interrupted and reported.
async fn agent(machine: Machine) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let uv = moil_runtime::find_uv()
        .context("couldn't find uv; install it (https://docs.astral.sh/uv/) or set $MOIL_UV")?;
    let runtime = Runtime::new(machine.runtime_config(uv));
    let reaped = runtime
        .reap_orphans()
        .context("couldn't clean up after an earlier crash")?;
    if reaped > 0 {
        tracing::info!("cleaned up {reaped} attempts left over from a crash");
    }
    let services = machine.services()?;
    if services.is_empty() {
        tracing::warn!("no paired services yet; pair one with `moil pair URL`");
    }
    if machine.settings().paused {
        tracing::info!("paused: not taking new jobs");
    }

    let agent = Agent::start(machine, Arc::new(runtime));
    let reviews = tokio::spawn(announce_reviews(agent.subscribe()));
    interrupted().await?;
    tracing::info!("shutting down");
    agent.shutdown().await;
    reviews.abort();
    Ok(())
}

/// Tells the owner, once each, about bundles waiting for their review.
async fn announce_reviews(mut snapshots: tokio::sync::watch::Receiver<Snapshot>) {
    let mut announced = HashSet::new();
    loop {
        for (service, bundle) in snapshots.borrow_and_update().pending_reviews() {
            if announced.insert((service.id.clone(), bundle.info.hash)) {
                tracing::info!(
                    "{} offers {} {}, which you haven't approved; see it with `moil review {} {}`",
                    service.name,
                    bundle.info.name,
                    bundle.info.version,
                    service.id,
                    bundle.info.hash
                );
            }
        }
        if snapshots.changed().await.is_err() {
            return;
        }
    }
}

async fn interrupted() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

/// Prints aligned columns. Only the last column may contain spaces.
fn table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == N {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  ", width = widths[i]));
            }
        }
        println!("{}", out.trim_end());
    };
    line(headers.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

/// A size for people: `75.5 MB`.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(512), "512 bytes");
        assert_eq!(size(75_538_270), "75.5 MB");
        assert_eq!(size(8_589_934_592), "8.6 GB");
    }
}
