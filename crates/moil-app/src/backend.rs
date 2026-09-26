//! What the app needs from the machine agent, as a trait, so that the same
//! UI runs on the real agent ([`AgentHandle`]) or on the demo
//! ([`crate::demo::Demo`]).

use std::sync::Arc;
use std::time::Instant;

use futures::FutureExt;
use futures::future::BoxFuture;
use moil_agent::{
    AgentHandle, Error, Home, Machine, MachineConfig, PairedService, Review, ServiceId, Snapshot,
    default_secrets,
};
use moil_proto::BundleHash;
use moil_runtime::{Runtime, find_uv};
use tokio::sync::watch;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The machine agent, as the app uses it. Methods mirror [`AgentHandle`]'s;
/// futures own their arguments so they can run on any task.
pub trait Backend: Send + Sync + 'static {
    /// Watches the agent's state; the receiver holds the current snapshot.
    fn subscribe(&self) -> watch::Receiver<Snapshot>;

    /// Starts pairing with the service at `base_url`: checks it is a moil
    /// service and asks it for a code for the owner to confirm.
    fn pair(&self, base_url: String) -> BoxFuture<'_, Result<Pairing>>;

    /// Fetches a bundle and verifies it against `hash`, with the approved
    /// version it would replace.
    fn review(&self, service: ServiceId, hash: BundleHash) -> BoxFuture<'_, Result<Review>>;

    /// Approves exactly the reviewed bundle.
    fn approve(&self, review: Review) -> BoxFuture<'_, Result<()>>;

    fn revoke(&self, service: ServiceId, hash: BundleHash) -> BoxFuture<'_, Result<()>>;

    /// Disconnects from a service and forgets it.
    fn unpair(&self, service: ServiceId) -> BoxFuture<'_, Result<()>>;

    /// Stops or resumes taking new jobs. A running job carries on.
    fn set_paused(&self, paused: bool);

    /// Stops the running job, if any; its service may give it to another
    /// machine.
    fn stop_job(&self);

    /// Stops the running job, reports it, and closes every connection.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

/// A pairing waiting for the owner to confirm its code on the service.
pub struct Pairing {
    pub code: PairingCode,
    /// Resolves once the owner confirms or rejects the pairing on the
    /// service, or the code expires. Dropping it abandons the pairing.
    pub confirmed: BoxFuture<'static, Result<PairedService>>,
}

impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

/// What the owner needs to see to confirm a pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    /// The name the service gives itself.
    pub service_name: String,
    pub expires_at: Instant,
}

impl Backend for AgentHandle {
    fn subscribe(&self) -> watch::Receiver<Snapshot> {
        AgentHandle::subscribe(self)
    }

    fn pair(&self, base_url: String) -> BoxFuture<'_, Result<Pairing>> {
        async move {
            let session = AgentHandle::pair(self, &base_url).await?;
            let code = PairingCode {
                user_code: session.user_code.clone(),
                verification_uri: session.verification_uri.clone(),
                verification_uri_complete: session.verification_uri_complete.clone(),
                service_name: session.service_name.clone(),
                expires_at: session.expires_at,
            };
            Ok(Pairing {
                code,
                confirmed: session.wait().boxed(),
            })
        }
        .boxed()
    }

    fn review(&self, service: ServiceId, hash: BundleHash) -> BoxFuture<'_, Result<Review>> {
        async move { AgentHandle::review(self, &service, hash).await }.boxed()
    }

    fn approve(&self, review: Review) -> BoxFuture<'_, Result<()>> {
        async move { AgentHandle::approve(self, &review).await }.boxed()
    }

    fn revoke(&self, service: ServiceId, hash: BundleHash) -> BoxFuture<'_, Result<()>> {
        async move { AgentHandle::revoke(self, &service, hash).await }.boxed()
    }

    fn unpair(&self, service: ServiceId) -> BoxFuture<'_, Result<()>> {
        async move { AgentHandle::unpair(self, &service).await }.boxed()
    }

    fn set_paused(&self, paused: bool) {
        AgentHandle::set_paused(self, paused);
    }

    fn stop_job(&self) {
        AgentHandle::stop_job(self);
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        AgentHandle::shutdown(self).boxed()
    }
}

/// Why the agent couldn't start, phrased for the owner.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Agent(#[from] Error),
    #[error(
        "moil can't find uv, which it needs to run jobs. Reinstall Moil.app, or set MOIL_UV to the path of a uv binary."
    )]
    NoUv,
}

/// Opens this computer's moil home and starts the agent on it, the way the
/// app runs for real. Must be called within a Tokio runtime.
pub fn start_agent() -> Result<Arc<dyn Backend>, StartError> {
    let home = Home::new(Home::default_root()?);
    let secrets = default_secrets(&home)?;
    let machine = Machine::open(MachineConfig {
        home,
        secrets,
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
    })?;
    let uv = find_uv().ok_or(StartError::NoUv)?;
    let settings = machine.settings();
    let mut config = machine.home().runtime_config(uv);
    config.allow_insecure_http = settings.allow_insecure_http;
    config.keep_work_dirs = settings.keep_work_dirs;
    let runtime = Runtime::new(config);
    match runtime.reap_orphans() {
        Ok(0) => {}
        Ok(n) => tracing::info!("cleaned up {n} attempts left over from an earlier run"),
        Err(e) => tracing::warn!("couldn't clean up attempts left over from an earlier run: {e}"),
    }
    Ok(Arc::new(moil_agent::Agent::start(
        machine,
        Arc::new(runtime),
    )))
}
