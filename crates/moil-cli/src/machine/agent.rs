//! `moil agent`: the agent, run in a terminal as the app runs it
//! (spec/machine.md §4).

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context as _;
use moil_agent::control::HostControls;
use moil_agent::display::reveal;
use moil_agent::host::{HostConfig, Hosted};
use moil_agent::report::Host;
use moil_agent::{AgentOptions, Snapshot};
use moil_runtime::Runtime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::Context;

/// What `moil quit` can ask of this process: to stop, as Ctrl-C does. Only
/// the app opens at login, so it keeps the default answer to that.
struct Controls {
    quit: CancellationToken,
}

impl HostControls for Controls {
    fn quit(&self) {
        self.quit.cancel();
    }
}

/// Runs the agent with the real runtime until SIGINT, SIGTERM or `moil
/// quit`, then shuts it down: the running job is interrupted and reported.
pub async fn run(ctx: &Context) -> anyhow::Result<()> {
    // Before opening the home, as the app does, so that what opening it
    // says is logged.
    moil_agent::log::init(Some(&ctx.home.log_file()));
    let machine = ctx.owner()?;
    let uv = moil_runtime::find_uv()
        .context("couldn't find uv; install it (https://docs.astral.sh/uv/) or set $MOIL_UV")?;
    let runtime = Runtime::new(machine.runtime_config(uv))?;
    if machine.services()?.iter().all(|s| s.pairing.is_none()) {
        tracing::warn!("no paired services yet; pair one with `moil pair URL`");
    }
    if machine.settings().paused {
        tracing::info!("paused: not taking new jobs until `moil resume`");
    }
    let moil = std::env::current_exe().context("couldn't tell where this moil is")?;
    let quit = CancellationToken::new();
    let hosted = Hosted::start(HostConfig {
        machine,
        runtime,
        moil,
        host: Host::Agent,
        controls: Arc::new(Controls { quit: quit.clone() }),
        options: AgentOptions::default(),
    })
    .await?;
    tracing::info!(
        "running on {}; moil commands reach this agent until it stops",
        ctx.home.root().display()
    );

    let reviews = tokio::spawn(announce_reviews(hosted.agent().subscribe()));
    tokio::select! {
        interrupted = interrupted() => interrupted?,
        () = quit.cancelled() => {}
    }
    tracing::info!("shutting down");
    hosted.shutdown().await;
    reviews.abort();
    Ok(())
}

/// Tells the owner, once each, about bundles waiting for their review.
async fn announce_reviews(mut snapshots: watch::Receiver<Snapshot>) {
    let mut announced = HashSet::new();
    loop {
        for (service, bundle) in snapshots.borrow_and_update().pending_reviews() {
            if announced.insert((service.id.clone(), bundle.info.hash)) {
                let (id, hash) = (&service.id, bundle.info.hash);
                tracing::info!(
                    "{} offers {} {}, which you haven't approved; read it with \
                     `moil review {id} {hash}`, and approve it with `moil approve {id} {hash}`",
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
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}
