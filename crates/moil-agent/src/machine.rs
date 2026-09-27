//! This computer's moil state.

use std::fmt;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use moil_bundle::Bundle;
use moil_proto::BundleHash;
use moil_proto::control::{BundleInfo, MachineInfo};
use moil_proto::http::{BundleResponse, BundlesResponse, InfoResponse, PairRequest, PairResponse};
use url::Url;

use crate::fs::{PUBLIC, read_json, with_path, write_json};
use crate::hardware::Hardware;
use crate::net::{self, Net};
use crate::services;
use crate::{
    Approval, Error, Home, PairedService, PairingSession, Review, SecretStore, ServiceId, Settings,
};

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
///
/// While an [`Agent`](crate::Agent) runs on a machine, pair, unpair,
/// approve and revoke through its [`AgentHandle`](crate::AgentHandle),
/// which also tells the services; changing them here alone leaves the
/// agent unaware until it restarts.
#[derive(Debug, Clone)]
pub struct Machine {
    inner: Arc<Inner>,
}

struct Inner {
    home: Home,
    secrets: Arc<dyn SecretStore>,
    app_version: String,
    settings: Mutex<Settings>,
    /// Held while a service's file is read, changed and written back.
    services: Mutex<()>,
    net: Net,
    /// Makes token keys unique to this home: the keychain is shared by
    /// every home on the computer.
    token_scope: String,
    hardware: OnceLock<Hardware>,
    _lock: File,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Machine")
            .field("home", &self.home.root())
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

/// A paired service and the token that proves this machine to it.
struct Credentials {
    service: PairedService,
    token: String,
}

/// How long unpairing waits for the service before forgetting it anyway.
const UNPAIR_TIMEOUT: Duration = Duration::from_secs(10);

impl Machine {
    /// Opens the machine's home, creating it if needed. Fails with
    /// [`Error::AlreadyRunning`] if another process has it open.
    pub fn open(config: MachineConfig) -> Result<Self, Error> {
        let MachineConfig {
            home,
            secrets,
            app_version,
        } = config;
        let lock = home.lock()?;
        let settings = home.load_settings()?;
        let token_scope = home
            .root()
            .canonicalize()
            .map_err(|e| with_path(e, home.root()))?
            .display()
            .to_string();
        Ok(Self {
            inner: Arc::new(Inner {
                net: Net::new(&app_version)?,
                home,
                secrets,
                app_version,
                settings: Mutex::new(settings),
                services: Mutex::new(()),
                token_scope,
                hardware: OnceLock::new(),
                _lock: lock,
            }),
        })
    }

    pub fn home(&self) -> &Home {
        &self.inner.home
    }

    /// The version of the app this machine runs, as services see it.
    pub fn app_version(&self) -> &str {
        &self.inner.app_version
    }

    pub fn settings(&self) -> Settings {
        self.lock_settings().clone()
    }

    /// Changes the settings and saves them, returning the new ones.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings, Error> {
        let mut settings = self.lock_settings();
        let mut changed = settings.clone();
        change(&mut changed);
        if changed != *settings {
            self.inner.home.save_settings(&changed)?;
            *settings = changed;
        }
        Ok(settings.clone())
    }

    /// The runtime's configuration for this machine: its home's
    /// directories, `uv` as the uv binary, and the owner's settings
    /// (`allow_insecure_http`, `keep_work_dirs`) as they are now.
    pub fn runtime_config(&self, uv: PathBuf) -> moil_runtime::RuntimeConfig {
        let settings = self.settings();
        let mut config = self.inner.home.runtime_config(uv);
        config.allow_insecure_http = settings.allow_insecure_http;
        config.keep_work_dirs = settings.keep_work_dirs;
        config
    }

    fn lock_settings(&self) -> MutexGuard<'_, Settings> {
        self.inner
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The machine as `hello` describes it: its name and hardware.
    pub fn info(&self) -> MachineInfo {
        let hardware = self.inner.hardware.get_or_init(Hardware::detect);
        MachineInfo {
            name: self.name(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            cpus: hardware.cpus,
            memory_bytes: hardware.memory_bytes,
            gpus: hardware.gpus.clone(),
        }
    }

    /// What services call this machine: the owner's choice, else the
    /// computer's name, at most 64 characters (spec §4).
    fn name(&self) -> String {
        let name = self
            .settings()
            .machine_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                let hardware = self.inner.hardware.get_or_init(Hardware::detect);
                hardware.computer_name.clone()
            });
        name.trim().chars().take(64).collect()
    }

    /// Every paired service, by ID.
    pub fn services(&self) -> Result<Vec<PairedService>, Error> {
        let dir = self.inner.home.services_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(with_path(e, &dir).into()),
        };
        let mut services = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| with_path(e, &dir))?.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            match read_json::<PairedService>(&path) {
                Ok(Some(service)) => services.push(service),
                Ok(None) => {}
                // One damaged file shouldn't cut the machine off from every
                // other service.
                Err(e) => tracing::warn!("skipping a service: {e}"),
            }
        }
        services.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(services)
    }

    /// The paired service with this ID.
    pub fn service(&self, id: &ServiceId) -> Result<PairedService, Error> {
        read_json(&self.inner.home.service_file(id))?
            .ok_or_else(|| Error::UnknownService(id.to_string()))
    }

    /// The paired service `query` names: its ID, host, base URL or name.
    pub fn find_service(&self, query: &str) -> Result<PairedService, Error> {
        let query = query.trim();
        let services = self.services()?;
        if let Some(service) = services.iter().find(|s| s.id.as_str() == query) {
            return Ok(service.clone());
        }
        let url = query
            .contains("://")
            .then(|| services::parse_base_url(query).ok())
            .flatten();
        let matches: Vec<&PairedService> = services
            .iter()
            .filter(|s| {
                let host = s.base_url.host_str().unwrap_or_default();
                let host_port = s
                    .base_url
                    .port_or_known_default()
                    .map(|port| format!("{host}:{port}"));
                url.as_ref() == Some(&s.base_url)
                    || host.eq_ignore_ascii_case(query)
                    || host_port.is_some_and(|hp| hp.eq_ignore_ascii_case(query))
                    || s.name.eq_ignore_ascii_case(query)
            })
            .collect();
        match matches.as_slice() {
            [] => Err(Error::UnknownService(query.to_owned())),
            [service] => Ok((*service).clone()),
            several => Err(Error::AmbiguousService {
                query: query.to_owned(),
                matches: several
                    .iter()
                    .map(|s| format!("{} ({})", s.id, s.base_url))
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }

    /// Starts pairing with the service at `base_url`, after checking that it
    /// is a moil service (`GET /v1/info`).
    pub async fn pair(&self, base_url: &str) -> Result<PairingSession, Error> {
        let base_url = services::parse_base_url(base_url)?;
        self.check_url(&base_url)?;
        let not_a_service = |reason: String| Error::NotAService {
            url: base_url.to_string(),
            reason,
        };

        let info_url = net::endpoint(&base_url, "v1/info");
        let response = self
            .net()
            .http
            .get(&info_url)
            .send()
            .await
            .map_err(|e| net::unreachable(base_url.as_str(), &e))?;
        if !response.status().is_success() {
            return Err(not_a_service(format!(
                "GET {info_url} answered {}",
                response.status()
            )));
        }
        let info: InfoResponse = net::read_json(base_url.as_str(), response)
            .await
            .map_err(|_| not_a_service(format!("{info_url} doesn't describe a moil service")))?;
        if info.protocol != moil_proto::PROTOCOL_VERSION {
            return Err(not_a_service(format!(
                "it speaks moil protocol {}, and this app speaks {}",
                info.protocol,
                moil_proto::PROTOCOL_VERSION
            )));
        }
        services::check_name(&info.name).map_err(not_a_service)?;

        // The computer's name may take detecting.
        let name = self.blocking(|machine| Ok(machine.name())).await?;
        let request = PairRequest {
            name,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            app_version: self.inner.app_version.clone(),
        };
        let response = self
            .net()
            .http
            .post(net::endpoint(&base_url, "v1/pair"))
            .json(&request)
            .send()
            .await
            .map_err(|e| net::unreachable(base_url.as_str(), &e))?;
        let started = Instant::now();
        let pairing: PairResponse = net::read_json(&info.name, response).await?;
        services::check_name(&pairing.service.name).map_err(not_a_service)?;
        for uri in [
            &pairing.verification_uri,
            &pairing.verification_uri_complete,
        ] {
            if !Url::parse(uri).is_ok_and(|u| matches!(u.scheme(), "https" | "http")) {
                return Err(not_a_service(format!(
                    "its confirmation page {uri:?} isn't a web address"
                )));
            }
        }
        Ok(PairingSession::new(
            self.clone(),
            base_url,
            pairing,
            started,
        ))
    }

    /// Records a confirmed pairing: stores the token and the service. Pairing
    /// a base URL again keeps its ID and approvals and replaces its token,
    /// which is returned so the caller can let the service forget it.
    pub(crate) fn record_pairing(
        &self,
        base_url: &Url,
        name: String,
        machine_id: String,
        token: &str,
    ) -> Result<(PairedService, Option<String>), Error> {
        let _services = self.lock_services();
        let services = self.services()?;
        let existing = services.iter().find(|s| s.base_url == *base_url);
        let (id, approvals, old_token) = match existing {
            Some(service) => (
                service.id.clone(),
                service.approvals.clone(),
                self.token(&service.id)?,
            ),
            None => {
                let taken = |id: &str| services.iter().any(|s| s.id.as_str() == id);
                (ServiceId::derive(base_url, taken), Vec::new(), None)
            }
        };
        self.inner.secrets.set(&self.token_key(&id), token)?;
        let service = PairedService {
            id,
            base_url: base_url.clone(),
            name,
            machine_id,
            paired_at: SystemTime::now(),
            approvals,
        };
        self.save_service(&service)?;
        Ok((service, old_token.filter(|old| old != token)))
    }

    /// Forgets a service: tells it to remove this machine
    /// (`DELETE /v1/machine`, best effort), then deletes its token and
    /// approvals here.
    pub async fn unpair(&self, id: &ServiceId) -> Result<(), Error> {
        let found = id.clone();
        let (service, token) = self
            .blocking(move |machine| Ok((machine.service(&found)?, machine.token(&found)?)))
            .await?;
        if let Some(token) = token {
            self.forget_token_on(&service.base_url, &token).await;
        }
        let id = id.clone();
        self.blocking(move |machine| machine.forget(&id, &service))
            .await
    }

    /// Deletes a service's token and approvals here.
    fn forget(&self, id: &ServiceId, service: &PairedService) -> Result<(), Error> {
        let _services = self.lock_services();
        self.inner.secrets.delete(&self.token_key(id))?;
        let path = self.inner.home.service_file(id);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(with_path(e, &path).into());
            }
            _ => {}
        }
        for approval in &service.approvals {
            self.drop_unused_bundle(&approval.hash)?;
        }
        Ok(())
    }

    /// Asks the service to remove the machine that `token` belongs to. Best
    /// effort: failures are only logged.
    pub(crate) async fn forget_token_on(&self, base_url: &Url, token: &str) {
        let request = self
            .net()
            .http
            .delete(net::endpoint(base_url, "v1/machine"))
            .bearer_auth(token)
            .timeout(UNPAIR_TIMEOUT)
            .send();
        match request.await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => tracing::warn!(
                "{base_url} answered {} when asked to forget this machine",
                response.status()
            ),
            Err(e) => tracing::warn!("couldn't tell {base_url} to forget this machine: {e}"),
        }
    }

    /// The bundles a service may send jobs for (`GET /v1/bundles`), as the
    /// service describes them.
    pub async fn bundles(&self, id: &ServiceId) -> Result<Vec<BundleInfo>, Error> {
        let credentials = self.credentials(id).await?;
        let response = self.get(&credentials, "v1/bundles").await?;
        let listing: BundlesResponse = net::read_json(&credentials.service.name, response).await?;
        Ok(listing.bundles)
    }

    /// Fetches a bundle for the owner to review and verifies it against
    /// `hash`.
    pub async fn review(&self, id: &ServiceId, hash: BundleHash) -> Result<Review, Error> {
        let credentials = self.credentials(id).await?;
        let response = self
            .get(&credentials, &format!("v1/bundles/{hash}"))
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::UnknownBundle(hash));
        }
        let service = credentials.service;
        let response: BundleResponse = net::read_json(&service.name, response).await?;
        self.blocking(move |machine| {
            let bundle = Bundle::from_response(response, &hash)?;
            let previous = machine.previous_version(&service, &bundle);
            Ok(Review {
                approved: service.is_approved(&hash),
                service,
                bundle,
                previous,
            })
        })
        .await
    }

    /// The most recently approved bundle from `service` with `bundle`'s
    /// name, other than `bundle` itself.
    fn previous_version(&self, service: &PairedService, bundle: &Bundle) -> Option<Bundle> {
        let store = self.inner.home.bundles();
        service
            .approvals
            .iter()
            .rev()
            .filter(|a| a.name == bundle.manifest().name && a.hash != bundle.hash())
            .find_map(|a| match store.get(&a.hash) {
                Ok(stored) => stored,
                Err(e) => {
                    tracing::warn!("the stored copy of {} is damaged: {e}", a.hash);
                    None
                }
            })
    }

    /// Approves exactly the reviewed bundle for the reviewed service and
    /// stores it.
    pub fn approve(&self, review: &Review) -> Result<PairedService, Error> {
        let _services = self.lock_services();
        let mut service = self.service(&review.service.id)?;
        if service.base_url != review.service.base_url {
            return Err(Error::UnknownService(format!(
                "{} at {}",
                review.service.id, review.service.base_url
            )));
        }
        let bundle = &review.bundle;
        self.inner
            .home
            .bundles()
            .put(bundle)
            .map_err(|e| with_path(e, &self.inner.home.bundles().dir(&bundle.hash())))?;
        if !service.is_approved(&bundle.hash()) {
            service.approvals.push(Approval {
                hash: bundle.hash(),
                name: bundle.manifest().name.clone(),
                version: bundle.manifest().version.clone(),
                approved_at: SystemTime::now(),
            });
            self.save_service(&service)?;
        }
        Ok(service)
    }

    /// Withdraws an approval. The stored copy of the bundle is deleted once
    /// no service's approval needs it. Revoking a bundle that isn't approved
    /// changes nothing.
    pub fn revoke(&self, id: &ServiceId, hash: BundleHash) -> Result<PairedService, Error> {
        let _services = self.lock_services();
        let mut service = self.service(id)?;
        if service.is_approved(&hash) {
            service.approvals.retain(|a| a.hash != hash);
            self.save_service(&service)?;
            self.drop_unused_bundle(&hash)?;
        }
        Ok(service)
    }

    /// The stored copy of a bundle approved for a service, verified.
    pub fn approved_bundle(
        &self,
        id: &ServiceId,
        hash: BundleHash,
    ) -> Result<Option<Bundle>, Error> {
        if !self.service(id)?.is_approved(&hash) {
            return Ok(None);
        }
        Ok(self.inner.home.bundles().get(&hash)?)
    }

    /// The machine token for a service, if it has one.
    pub(crate) fn token(&self, id: &ServiceId) -> Result<Option<String>, Error> {
        self.inner.secrets.get(&self.token_key(id))
    }

    /// Where a service's token is kept. One key per service ID, so pairing
    /// the same service again replaces the token.
    fn token_key(&self, id: &ServiceId) -> String {
        format!("{id}@{}", self.inner.token_scope)
    }

    pub(crate) fn net(&self) -> &Net {
        &self.inner.net
    }

    /// Fails unless the owner's settings allow reaching `url`.
    pub(crate) fn check_url(&self, url: &Url) -> Result<(), Error> {
        net::check_url(url, self.settings().allow_insecure_http)
    }

    /// Runs `work` where blocking is fine. State files take a moment to
    /// read and write, and the keychain may even ask the owner first; on an
    /// async worker thread either would hold up everything scheduled there.
    pub(crate) async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Machine) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let machine = self.clone();
        match tokio::task::spawn_blocking(move || work(&machine)).await {
            Ok(result) => result,
            Err(e) => match e.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(_) => Err(Error::Io(std::io::Error::other("moil is shutting down"))),
            },
        }
    }

    /// Service `id` and this machine's token for it.
    async fn credentials(&self, id: &ServiceId) -> Result<Credentials, Error> {
        let id = id.clone();
        self.blocking(move |machine| {
            let service = machine.service(&id)?;
            let token = machine.token(&id)?.ok_or(Error::Unauthorized)?;
            Ok(Credentials { service, token })
        })
        .await
    }

    /// `GET`s a machine-authenticated endpoint of a service.
    async fn get(&self, to: &Credentials, path: &str) -> Result<reqwest::Response, Error> {
        let service = &to.service;
        self.check_url(&service.base_url)?;
        let url = net::endpoint(&service.base_url, path);
        self.net()
            .http
            .get(&url)
            .bearer_auth(&to.token)
            .send()
            .await
            .map_err(|e| net::unreachable(service.base_url.as_str(), &e))
    }

    fn save_service(&self, service: &PairedService) -> Result<(), Error> {
        write_json(&self.inner.home.service_file(&service.id), service, PUBLIC)
    }

    fn lock_services(&self) -> MutexGuard<'_, ()> {
        self.inner
            .services
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Deletes a stored bundle unless some service still approves it.
    fn drop_unused_bundle(&self, hash: &BundleHash) -> Result<(), Error> {
        if !self.services()?.iter().any(|s| s.is_approved(hash)) {
            let store = self.inner.home.bundles();
            store
                .remove(hash)
                .map_err(|e| with_path(e, &store.dir(hash)))?;
        }
        Ok(())
    }
}
