//! Commands for machine owners.
//!
//! What scripts and the end-to-end tests read goes to stdout in stable
//! `Label: value` lines (`Code: WDJB-MJHT`, `Paired: klisi.example.com`)
//! or columns; everything else, including prompts, goes to stderr.

use std::collections::HashSet;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Command as Process, Stdio};
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Subcommand;
use moil_agent::display::{self, reveal};
use moil_agent::{
    Agent, Home, Machine, MachineConfig, PairedService, ReadOnlyMachine, Review, Snapshot,
    default_secrets,
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
    // Commands that only read work while the agent runs, without the
    // home's lock. The others change what the agent holds.
    let reader =
        || -> anyhow::Result<ReadOnlyMachine> { Ok(Machine::open_read_only(config(globals)?)?) };
    let owner = || -> anyhow::Result<Machine> { Ok(Machine::open(config(globals)?)?) };
    match command {
        Command::Pair {
            url,
            name,
            no_browser,
        } => pair(&owner()?, &url, name, no_browser).await,
        Command::Services => services(&reader()?),
        Command::Unpair { service } => {
            let machine = owner()?;
            let service = machine.find_service(&service)?;
            machine.unpair(&service.id).await?;
            println!("Unpaired: {}", service.id);
            Ok(())
        }
        Command::Review {
            service,
            hash: None,
        } => {
            let machine = reader()?;
            bundles(&machine, &machine.find_service(&service)?).await
        }
        Command::Review {
            service,
            hash: Some(hash),
        } => {
            let machine = reader()?;
            let service = machine.find_service(&service)?;
            let review = machine.review(&service.id, hash).await?;
            Ok(show(&review, &mut io::stdout().lock())?)
        }
        Command::Approve { service, hash, yes } => {
            let machine = owner()?;
            let service = machine.find_service(&service)?;
            approve(&machine, &machine.review(&service.id, hash).await?, yes)
        }
        Command::Revoke { service, hash } => {
            let machine = owner()?;
            let service = machine.find_service(&service)?;
            if !service.is_approved(&hash) {
                bail!("{hash} isn't approved for {}", service.id);
            }
            machine.revoke(&service.id, hash)?;
            println!("Revoked: {hash}");
            Ok(())
        }
        Command::Agent => agent(owner()?).await,
    }
}

/// The machine's home, as `--home` or `$MOIL_HOME` say, and its tokens.
fn config(globals: &Globals) -> anyhow::Result<MachineConfig> {
    let root = match &globals.home {
        Some(root) => root.clone(),
        None => Home::default_root()?,
    };
    let home = Home::new(root);
    let secrets = default_secrets(&home)?;
    Ok(MachineConfig {
        home,
        secrets,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
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
        reveal(&session.service_name),
        session.base_url(),
        machine.info().name
    );
    if session.base_url().scheme() == "http" {
        eprintln!("Warning: this service is reached over plain http.");
    }
    println!("Code: {}", reveal(&session.user_code));
    println!("URL: {}", reveal(&session.verification_uri_complete));
    if !no_browser && open_browser(&session.verification_uri_complete).is_err() {
        eprintln!("Couldn't open a browser; open the URL above yourself.");
    }
    eprintln!(
        "Check that {} shows the same code, then confirm it there. Waiting…",
        reveal(&session.service_name)
    );
    let service = session.wait().await?;
    println!("Paired: {}", service.id);
    eprintln!(
        "{} can now offer this machine jobs. See its bundles with `moil review {}`.",
        reveal(&service.name),
        service.id
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

fn services(machine: &ReadOnlyMachine) -> anyhow::Result<()> {
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
                reveal(&s.name).into_owned(),
            ]
        })
        .collect();
    table(&["ID", "URL", "APPROVED", "NAME"], &rows);
    Ok(())
}

/// The service's bundles and whether each is approved, plus approved
/// bundles it no longer offers.
async fn bundles(machine: &ReadOnlyMachine, service: &PairedService) -> anyhow::Result<()> {
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
                format!("{} {}", reveal(&b.name), reveal(&b.version)),
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
                    format!("{} {}", reveal(&a.name), reveal(&a.version)),
                ]
            }),
    );
    if rows.is_empty() {
        eprintln!("{} offers no bundles.", reveal(&service.name));
        return Ok(());
    }
    table(&["HASH", "STATUS", "BUNDLE"], &rows);
    eprintln!("Show one in full with `moil review {} HASH`.", service.id);
    Ok(())
}

/// Everything the owner needs to decide (spec §5.5): the service, the three
/// files in full, the packages, the hash and how to check it, and what
/// changed since the version approved before. Everything the service or the
/// bundle controls goes through [`reveal`], so no character in it can
/// rewrite what the owner reads.
fn show(review: &Review, out: &mut impl Write) -> io::Result<()> {
    let (bundle, service) = (&review.bundle, &review.service);
    let manifest = bundle.manifest();
    let (name, version) = (reveal(&manifest.name), reveal(&manifest.version));
    let service_name = reveal(&service.name);
    writeln!(
        out,
        "Service: {service_name} ({})",
        reveal(service.base_url.as_str())
    )?;
    if service.is_insecure() {
        writeln!(out, "Warning: this service is reached over plain http.")?;
    }
    writeln!(out, "Bundle: {name} {version}")?;
    if let Some(description) = &manifest.description {
        writeln!(out, "Description: {}", reveal(description))?;
    }
    writeln!(out, "Hash: {}", bundle.hash())?;
    let status = if review.approved {
        "approved"
    } else {
        "not approved"
    };
    writeln!(out, "Status: {status}")?;
    if let Some(memory) = manifest.resources.memory_bytes {
        writeln!(out, "Memory: {}", display::memory(memory))?;
    }
    if let Some(gpu) = manifest.resources.gpu {
        writeln!(out, "GPU: {}", format!("{gpu:?}").to_lowercase())?;
    }
    for (asset_name, asset) in &manifest.assets {
        let size = asset
            .size_bytes
            .map(display::size)
            .unwrap_or_else(|| "size unknown".into());
        writeln!(
            out,
            "Asset: {} ({size}) from {}, sha256 {}",
            reveal(asset_name),
            reveal(&asset.url),
            reveal(&asset.sha256)
        )?;
    }

    writeln!(out)?;
    writeln!(
        out,
        "To check the hash yourself, save the three files below into one directory and run"
    )?;
    writeln!(out, "  {}", Review::VERIFY_COMMAND)?;
    writeln!(out, "It must print {}.", bundle.hash())?;
    for (file, text) in bundle.files() {
        writeln!(out)?;
        writeln!(out, "==> {file} <==")?;
        write!(out, "{}", reveal(text))?;
        if !text.ends_with('\n') {
            writeln!(out)?;
            writeln!(out, "(no newline at the end of {file})")?;
        }
    }

    writeln!(out)?;
    let packages = bundle.packages();
    writeln!(out, "Packages ({}):", packages.len())?;
    for package in packages {
        let source = match (&package.source, package.link()) {
            (_, Some(link)) => link,
            (
                PackageSource::Registry(index)
                | PackageSource::Git(index)
                | PackageSource::Url(index),
                None,
            ) => {
                format!("from {index}")
            }
        };
        let built = if package.builds_from_source {
            "  (built from source here; its build tools aren't pinned)"
        } else {
            ""
        };
        writeln!(
            out,
            "  {} {}  {}{built}",
            reveal(&package.name),
            reveal(package.version.as_deref().unwrap_or("(no version)")),
            reveal(&source)
        )?;
    }

    writeln!(out)?;
    match (&review.previous, review.diff()) {
        (Some(previous), Some(diffs)) => {
            writeln!(
                out,
                "Changes since {} {} ({}):",
                reveal(&previous.manifest().name),
                reveal(&previous.manifest().version),
                previous.hash()
            )?;
            for diff in diffs {
                if diff.is_unchanged() {
                    writeln!(out, "  {} is unchanged", diff.name)?;
                } else {
                    write!(out, "{}", reveal(&diff.unified()))?;
                }
            }
        }
        _ => writeln!(
            out,
            "No earlier version of {name} is approved for {service_name}."
        )?,
    }
    writeln!(out)?;
    writeln!(
        out,
        "Approving lets this code run on this machine with your permissions whenever \
         {service_name} sends it a job. moil has no sandbox yet."
    )
}

fn approve(machine: &Machine, review: &Review, yes: bool) -> anyhow::Result<()> {
    show(review, &mut io::stdout().lock())?;
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
            reveal(&manifest.name),
            reveal(&manifest.version),
            reveal(&review.service.name)
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
    // As in the app: leftovers are worth a warning, not refusing to work.
    match runtime.reap_orphans() {
        Ok(0) => {}
        Ok(n) => tracing::info!("cleaned up {n} attempts left over from a crash"),
        Err(e) => tracing::warn!("couldn't clean up attempts left over from a crash: {e}"),
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
/// Reading one works while the agent runs; approving changes what the agent
/// holds, so the owner stops it first.
async fn announce_reviews(mut snapshots: tokio::sync::watch::Receiver<Snapshot>) {
    let mut announced = HashSet::new();
    loop {
        for (service, bundle) in snapshots.borrow_and_update().pending_reviews() {
            if announced.insert((service.id.clone(), bundle.info.hash)) {
                let (id, hash) = (&service.id, bundle.info.hash);
                tracing::info!(
                    "{} offers {} {}, which you haven't approved; read it with \
                     `moil review {id} {hash}`, and to approve it, stop the agent and run \
                     `moil approve {id} {hash}`",
                    reveal(&service.name),
                    reveal(&bundle.info.name),
                    reveal(&bundle.info.version),
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

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use moil_agent::display::is_hidden;
    use moil_agent::{PairedService, ServiceId};
    use moil_bundle::Bundle;

    use super::*;

    /// A bundle, and the service offering it, that hide things everywhere
    /// they can: the screen-clearing escape sequence and the invisible
    /// characters would let a harmless-looking review be drawn over the
    /// real one.
    fn hostile() -> Review {
        let clear = "\x1b[2J\x1b[H";
        let script = format!(
            "# /// script\n# requires-python = \">=3.12\"\n# dependencies = []\n# ///\n\
             import os{clear}\nprint('hi')\u{202e}\r\n"
        );
        let hash = format!("sha256:{}", "a".repeat(64));
        let lockfile = format!(
            "version = 1\nrevision = 3\n\n[[package]]\nname = \"requests\\u001b[2J\"\n\
             version = \"2.32.3\\u001b[8m\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n\
             wheels = [{{ hash = \"{hash}\" }}]\n"
        );
        let manifest = "{\"moil\": 1, \"name\": \"echo\", \"version\": \"1.0\u{3164}\", \
                        \"description\": \"Harmless.\u{200b}\"}";
        let bundle = Bundle::new(script, lockfile, manifest).unwrap();
        let base_url = "https://klisi.example.com/moil".parse().unwrap();
        Review {
            service: PairedService {
                id: ServiceId::new("klisi.example.com").unwrap(),
                base_url,
                name: format!("klisi{clear}\u{2066}"),
                machine_id: "m_1".into(),
                paired_at: SystemTime::now(),
                approvals: vec![],
            },
            previous: Some(bundle.clone()),
            bundle,
            approved: false,
        }
    }

    #[test]
    fn a_review_shows_nothing_hidden() {
        let mut out = Vec::new();
        show(&hostile(), &mut out).unwrap();
        let shown = String::from_utf8(out).unwrap();
        let hidden: Vec<char> = shown.chars().filter(|&c| is_hidden(c)).collect();
        assert!(hidden.is_empty(), "{hidden:?} in\n{shown}");
        for revealed in [
            "Service: klisi␛[2J␛[H\\u{2066} (https://klisi.example.com/moil)",
            "Bundle: echo 1.0\\u{3164}",
            "Description: Harmless.\\u{200b}",
            "import os␛[2J␛[H",
            "print('hi')\\u{202e}␍",
            "  requests␛[2J 2.32.3␛[8m  https://pypi.org/project/requests␛[2J/2.32.3␛[8m/",
        ] {
            assert!(shown.contains(revealed), "{revealed:?} in\n{shown}");
        }
    }
}
