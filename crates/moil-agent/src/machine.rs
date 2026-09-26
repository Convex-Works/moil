//! This computer's moil state.

use std::sync::Arc;

use moil_bundle::Bundle;
use moil_proto::BundleHash;
use moil_proto::control::{BundleInfo, MachineInfo};

use crate::{Error, Home, PairedService, PairingSession, Review, SecretStore, ServiceId, Settings};

#[derive(Debug, Clone)]
pub struct MachineConfig {
    pub home: Home,
    pub secrets: Arc<dyn SecretStore>,
    /// Sent to services in `hello` and when pairing.
    pub app_version: String,
}

/// This computer's moil state: settings, paired services, approvals and
/// tokens. Opening it takes the home's lock, released when the last clone
/// is dropped. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Machine {
    #[allow(dead_code)]
    inner: Arc<()>,
}

impl Machine {
    /// Opens the machine's home, creating it if needed. Fails with
    /// [`Error::AlreadyRunning`] if another process has it open.
    pub fn open(_config: MachineConfig) -> Result<Self, Error> {
        todo!("moil-agent: Machine::open")
    }

    pub fn home(&self) -> &Home {
        todo!("moil-agent: Machine::home")
    }

    pub fn settings(&self) -> Settings {
        todo!("moil-agent: Machine::settings")
    }

    pub fn update_settings(&self, _change: impl FnOnce(&mut Settings)) -> Result<Settings, Error> {
        todo!("moil-agent: Machine::update_settings")
    }

    /// The machine as `hello` describes it: its name and hardware.
    pub fn info(&self) -> MachineInfo {
        todo!("moil-agent: Machine::info")
    }

    pub fn services(&self) -> Result<Vec<PairedService>, Error> {
        todo!("moil-agent: Machine::services")
    }

    /// The paired service `query` names: its ID, host, base URL or name.
    pub fn find_service(&self, _query: &str) -> Result<PairedService, Error> {
        todo!("moil-agent: Machine::find_service")
    }

    /// Starts pairing with the service at `base_url`, after checking that it
    /// is a moil service (`GET /v1/info`).
    pub async fn pair(&self, _base_url: &str) -> Result<PairingSession, Error> {
        todo!("moil-agent: Machine::pair")
    }

    /// Forgets a service: tells it to remove this machine
    /// (`DELETE /v1/machine`, best effort), then deletes its token and
    /// approvals here.
    pub async fn unpair(&self, _id: &ServiceId) -> Result<(), Error> {
        todo!("moil-agent: Machine::unpair")
    }

    /// The bundles a service may send jobs for (`GET /v1/bundles`), as the
    /// service describes them.
    pub async fn bundles(&self, _id: &ServiceId) -> Result<Vec<BundleInfo>, Error> {
        todo!("moil-agent: Machine::bundles")
    }

    /// Fetches a bundle for the owner to review and verifies it against
    /// `hash`.
    pub async fn review(&self, _id: &ServiceId, _hash: BundleHash) -> Result<Review, Error> {
        todo!("moil-agent: Machine::review")
    }

    /// Approves exactly the reviewed bundle for the reviewed service and
    /// stores it.
    pub fn approve(&self, _review: &Review) -> Result<PairedService, Error> {
        todo!("moil-agent: Machine::approve")
    }

    pub fn revoke(&self, _id: &ServiceId, _hash: BundleHash) -> Result<PairedService, Error> {
        todo!("moil-agent: Machine::revoke")
    }

    /// The stored copy of a bundle approved for a service, verified.
    pub fn approved_bundle(
        &self,
        _id: &ServiceId,
        _hash: BundleHash,
    ) -> Result<Option<Bundle>, Error> {
        todo!("moil-agent: Machine::approved_bundle")
    }
}
