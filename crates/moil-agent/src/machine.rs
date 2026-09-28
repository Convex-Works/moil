//! This computer's moil state.

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use moil_bundle::Bundle;
use moil_proto::BundleHash;
use moil_proto::control::{BundleInfo, MachineInfo};
use moil_proto::http::{BundleResponse, BundlesResponse, InfoResponse, PairRequest, PairResponse};
use moil_proto::ids::is_bundle_name;
use url::Url;

use crate::folders;
use crate::fs::{PUBLIC, read_strict_json, with_path, write_json};
use crate::hardware::Hardware;
use crate::net::{self, Net};
use crate::pairing::check_confirmation_page;
use crate::services::{self, ServiceFile};
use crate::{
    Approval, Error, Home, Pairing, PairingSession, Review, SecretStore, Service, ServiceId,
    Settings,
};

#[derive(Debug, Clone)]
pub struct MachineConfig {
    pub home: Home,
    pub secrets: Arc<dyn SecretStore>,
    /// Sent to services in `hello` and when pairing.
    pub app_version: String,
}

/// This computer's moil state: settings, the services its home knows and
/// its pairings with them, approvals and tokens. Opening it takes the
/// home's lock, released when the last clone is dropped. Cheap to clone.
///
/// While an [`Agent`](crate::Agent) runs on a machine, pair, unpair,
/// approve and revoke through its [`AgentHandle`](crate::AgentHandle),
/// which tells the services at once. The agent notices a change made here
/// alone as it notices one made from outside, within a few seconds, but
/// unpairing here alone doesn't tell the service.
#[derive(Debug, Clone)]
pub struct Machine {
    inner: Arc<Inner>,
}

struct Inner {
    home: Home,
    secrets: Arc<dyn SecretStore>,
    app_version: String,
    /// The settings moil goes by: as read when the machine was opened,
    /// changed through [`Machine::update_settings`], or read again once an
    /// agent noticed they changed.
    settings: Mutex<Settings>,
    /// Whether plain `http` is allowed: as `settings.json` said when the
    /// machine was opened, or as [`Machine::update_settings`] set it since.
    /// A change made from outside takes effect when moil next starts
    /// (spec/machine.md §1.1).
    allow_insecure_http: AtomicBool,
    /// Held while a service's files are read, changed and written back.
    services: Mutex<()>,
    net: Net,
    /// Makes token keys unique to this home: the keychain is shared by
    /// every home on the computer.
    token_scope: String,
    hardware: OnceLock<Hardware>,
    /// The home's lock, unless this is a [`ReadOnlyMachine`].
    _lock: Option<File>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Machine")
            .field("home", &self.home.root())
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

/// This computer's moil state, opened without taking the home's lock, for
/// looking at while an agent runs: the services the home knows, what's
/// wrong with it, and the bundles services offer. It changes nothing, and
/// reads each file afresh, so it sees what the agent has written. Cheap to
/// clone.
#[derive(Debug, Clone)]
pub struct ReadOnlyMachine(pub(crate) Machine);

impl ReadOnlyMachine {
    pub fn home(&self) -> &Home {
        self.0.home()
    }

    /// The settings as they were when the machine was opened: the
    /// defaults if `settings.json` isn't valid, which `moil status` then
    /// reports ([`status::from_home`](crate::status::from_home)).
    pub fn settings(&self) -> Settings {
        self.0.settings()
    }

    /// Like [`Machine::find_service`].
    pub fn find_service(&self, query: &str) -> Result<Service, Error> {
        self.0.find_service(query)
    }

    /// Like [`Machine::bundles`].
    pub async fn bundles(&self, id: &ServiceId) -> Result<Vec<BundleInfo>, Error> {
        self.0.bundles(id).await
    }

    /// Like [`Machine::review`].
    pub async fn review(&self, id: &ServiceId, hash: BundleHash) -> Result<Review, Error> {
        self.0.review(id, hash).await
    }
}

/// A paired service and the token that proves this machine to it.
struct Credentials {
    service: Service,
    /// The base URL the token was issued for: the only one it goes to.
    base_url: Url,
    token: String,
}

/// The most GPUs `hello` lists (spec §10).
const MAX_GPUS: usize = 64;

/// `text`, cut to the 64 characters services accept for the machine's
/// name, OS, architecture and app version (spec §10).
fn bounded(text: &str) -> String {
    text.chars().take(64).collect()
}

/// How long unpairing waits for the service before forgetting it anyway.
const UNPAIR_TIMEOUT: Duration = Duration::from_secs(10);

/// A change made to `settings.json` from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingsChange {
    /// The settings moil went by.
    pub before: Settings,
    /// What the file says now. Only the fields that differ from `before`
    /// changed: moil may yet write the others as it last chose them.
    pub after: Settings,
}

/// Makes the fields that differ between `before` and `after` the same in
/// `target`, leaving the others as they are.
fn carry_changes(before: &Settings, after: &Settings, target: &mut Settings) {
    let Settings {
        paused,
        machine_name,
        allow_insecure_http,
        keep_work_dirs,
    } = after;
    if before.paused != *paused {
        target.paused = *paused;
    }
    if before.machine_name != *machine_name {
        target.machine_name.clone_from(machine_name);
    }
    if before.allow_insecure_http != *allow_insecure_http {
        target.allow_insecure_http = *allow_insecure_http;
    }
    if before.keep_work_dirs != *keep_work_dirs {
        target.keep_work_dirs = *keep_work_dirs;
    }
}

impl Machine {
    /// Opens the machine's home, creating it if needed, and writes what
    /// the home's readers go by (`README.md` and the `.gitignore` files).
    /// Fails with [`Error::AlreadyRunning`] if another process has it open,
    /// and with [`Error::Invalid`] if `settings.json` isn't valid: a guess
    /// could take jobs the owner meant to pause (spec/machine.md §1.1).
    pub fn open(config: MachineConfig) -> Result<Self, Error> {
        let lock = config.home.lock()?;
        let settings = config.home.load_settings()?;
        let machine = Self::with_lock(config, settings, Some(lock))?;
        // Nothing moil does depends on them.
        if let Err(e) = machine.inner.home.write_guides() {
            tracing::warn!("couldn't write the home's README.md and .gitignore files: {e}");
        }
        machine.prune_bundle_store();
        Ok(machine)
    }

    /// Opens the machine's home to read it, even while another process has
    /// it open. Nothing is created or changed. An invalid `settings.json`
    /// reads as the defaults.
    pub fn open_read_only(config: MachineConfig) -> Result<ReadOnlyMachine, Error> {
        let settings = config.home.load_settings().unwrap_or_else(|e| {
            tracing::warn!("reading the home with the default settings: {e}");
            Settings::default()
        });
        Self::with_lock(config, settings, None).map(ReadOnlyMachine)
    }

    fn with_lock(
        config: MachineConfig,
        settings: Settings,
        lock: Option<File>,
    ) -> Result<Self, Error> {
        let MachineConfig {
            home,
            secrets,
            app_version,
        } = config;
        let app_version = bounded(&app_version);
        let token_scope = match home.root().canonicalize() {
            Ok(root) => root.display().to_string(),
            // A home that doesn't exist yet has no tokens to find.
            Err(e) if lock.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
                home.root().display().to_string()
            }
            Err(e) => return Err(with_path(e, home.root()).into()),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                net: Net::new(&app_version)?,
                home,
                secrets,
                app_version,
                allow_insecure_http: AtomicBool::new(settings.allow_insecure_http),
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

    /// The settings moil goes by. While an agent runs, they follow the
    /// changes it notices in `settings.json` (spec/machine.md §1.3).
    pub fn settings(&self) -> Settings {
        self.lock_settings().clone()
    }

    /// Changes the settings and saves them, returning the new ones. Only
    /// the fields `change` changes are written: whatever else someone
    /// changed in `settings.json` meanwhile stays, for an agent to notice.
    /// Fails without writing if `settings.json` isn't valid, since it's
    /// the owner's to fix.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<Settings, Error> {
        let mut settings = self.lock_settings();
        let mut changed = settings.clone();
        change(&mut changed);
        let on_disk = self.inner.home.load_settings()?;
        let mut written = on_disk.clone();
        carry_changes(&settings, &changed, &mut written);
        if written != on_disk {
            self.inner.home.save_settings(&written)?;
        }
        if changed.allow_insecure_http != settings.allow_insecure_http {
            self.inner
                .allow_insecure_http
                .store(changed.allow_insecure_http, Ordering::SeqCst);
        }
        *settings = changed;
        Ok(settings.clone())
    }

    /// Reads `settings.json` again, and goes by it if it changed since moil
    /// last read or wrote it: returns the change then, and `None`
    /// otherwise. `allow_insecure_http` and `keep_work_dirs` still take
    /// effect only when moil next starts (spec/machine.md §1.1). Fails
    /// with what's wrong with the file, keeping the settings as they were.
    pub(crate) fn reload_settings(&self) -> Result<Option<SettingsChange>, Error> {
        let mut settings = self.lock_settings();
        let on_disk = self.inner.home.load_settings()?;
        if on_disk == *settings {
            return Ok(None);
        }
        let before = std::mem::replace(&mut *settings, on_disk.clone());
        Ok(Some(SettingsChange {
            before,
            after: on_disk,
        }))
    }

    /// Whether the owner allows plain `http` services, inputs and outputs
    /// on any host (spec/machine.md §1.1).
    pub(crate) fn allows_insecure_http(&self) -> bool {
        self.inner.allow_insecure_http.load(Ordering::SeqCst)
    }

    /// The runtime's configuration for this machine: its home's
    /// directories, `uv` as the uv binary, and the owner's settings
    /// (`allow_insecure_http`, `keep_work_dirs`) as they are now.
    pub fn runtime_config(&self, uv: PathBuf) -> moil_runtime::RuntimeConfig {
        let settings = self.settings();
        let mut config = self.inner.home.runtime_config(uv);
        config.allow_insecure_http = self.allows_insecure_http();
        config.keep_work_dirs = settings.keep_work_dirs;
        config
    }

    fn lock_settings(&self) -> MutexGuard<'_, Settings> {
        self.inner
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The machine as `hello` describes it: its name and hardware, within
    /// what services accept (spec §10).
    pub fn info(&self) -> MachineInfo {
        let hardware = self.inner.hardware.get_or_init(Hardware::detect);
        MachineInfo {
            name: self.name(),
            os: bounded(std::env::consts::OS),
            arch: bounded(std::env::consts::ARCH),
            cpus: hardware.cpus,
            memory_bytes: hardware.memory_bytes,
            gpus: hardware.gpus.iter().take(MAX_GPUS).cloned().collect(),
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
        bounded(name.trim())
    }

    /// Every service the home knows, paired or not, by ID. Folders moil
    /// can't make sense of are left out; `moil status` reports them
    /// ([`status`](crate::status)).
    pub fn services(&self) -> Result<Vec<Service>, Error> {
        Ok(folders::scan(&self.inner.home)?.services)
    }

    /// The service with this ID. Fails with [`Error::Invalid`] if its
    /// `service.json` isn't valid.
    pub fn service(&self, id: &ServiceId) -> Result<Service, Error> {
        match folders::read(&self.inner.home, id).service {
            Some(service) => Ok(service),
            None => {
                // Says what's wrong, if the file is there.
                self.read_service_file(id)?;
                Err(Error::UnknownService(id.to_string()))
            }
        }
    }

    /// The service `query` names: its ID, host, base URL or name.
    pub fn find_service(&self, query: &str) -> Result<Service, Error> {
        let query = query.trim();
        let services = self.services()?;
        if let Some(service) = services.iter().find(|s| s.id.as_str() == query) {
            return Ok(service.clone());
        }
        let url = query
            .contains("://")
            .then(|| services::parse_base_url(query).ok())
            .flatten();
        let matches: Vec<&Service> = services
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
        check_confirmation_page(&base_url, &pairing.verification_uri)?;
        check_confirmation_page(&base_url, &pairing.verification_uri_complete)?;
        Ok(PairingSession::new(
            self.clone(),
            base_url,
            pairing,
            started,
        ))
    }

    /// Records a confirmed pairing: stores the token and `pairing.json`.
    /// Pairing with a base URL that a folder's `service.json` names adopts
    /// that folder, paired or not, keeping its approvals and hooks; else
    /// the service gets a new folder, under an ID no folder uses. Returns
    /// the service, and the token the new one replaced with the base URL it
    /// was issued for, so that the caller can let that service forget it.
    pub(crate) fn record_pairing(
        &self,
        base_url: &Url,
        name: String,
        machine_id: String,
        token: &str,
    ) -> Result<(Service, Option<(Url, String)>), Error> {
        let _services = self.lock_services();
        let home = &self.inner.home;
        // A folder this machine is paired through comes first, then the
        // first by ID.
        let adopted = self
            .services()?
            .into_iter()
            .filter(|s| s.base_url == *base_url)
            .min_by_key(|s| s.pairing.is_none());
        let (id, mut file) = match adopted {
            Some(service) => {
                let file = self.read_service_file(&service.id)?;
                (service.id, file)
            }
            None => {
                let taken = self.folder_names()?;
                let id = ServiceId::derive(base_url, |id| taken.contains(id));
                let file = ServiceFile {
                    base_url: base_url.clone(),
                    name: name.clone(),
                    approvals: Vec::new(),
                };
                self.save_service_file(&id, &file)?;
                (id, file)
            }
        };
        if let Err(e) = home.write_service_gitignore(&id) {
            tracing::warn!("couldn't write {id}'s .gitignore: {e}");
        }
        if file.name != name {
            file.name = name;
            self.save_service_file(&id, &file)?;
        }

        // The token this one replaces goes back only to where it was
        // issued, if that's known.
        let paired_with = read_strict_json::<Pairing>(&home.pairing_file(&id))
            .ok()
            .flatten()
            .map(|pairing| pairing.base_url);
        let replaced = match (self.token(&id)?, paired_with) {
            (Some(old), Some(url)) if old != token || url != *base_url => Some((url, old)),
            _ => None,
        };
        self.inner.secrets.set(&self.token_key(&id), token)?;
        let pairing = Pairing {
            base_url: base_url.clone(),
            machine_id,
            paired_at: crate::rfc3339::now(),
        };
        write_json(&home.pairing_file(&id), &pairing, PUBLIC)?;
        Ok((self.service(&id)?, replaced))
    }

    /// Unpairs from a service (spec/machine.md §3.2): tells it to remove
    /// this machine (`DELETE /v1/machine`, best effort, and only at the
    /// base URL the machine paired with), then deletes the token and
    /// `pairing.json`. The folder stays, with its approvals and hooks.
    pub async fn unpair(&self, id: &ServiceId) -> Result<(), Error> {
        let found = id.clone();
        let (pairing, token) = self
            .blocking(move |machine| {
                let home = &machine.inner.home;
                let pairing_file = home.pairing_file(&found);
                let pairing = read_strict_json::<Pairing>(&pairing_file).ok().flatten();
                let token = machine.token(&found)?;
                if !pairing_file.exists() && token.is_none() {
                    return Err(match home.service_dir(&found).is_dir() {
                        true => Error::NotPaired(found),
                        false => Error::UnknownService(found.to_string()),
                    });
                }
                Ok((pairing, token))
            })
            .await?;
        if let (Some(pairing), Some(token)) = (pairing, token) {
            self.forget_token_on(&pairing.base_url, &token).await;
        }
        let id = id.clone();
        self.blocking(move |machine| machine.forget(&id)).await
    }

    /// Deletes this machine's pairing with a service: its token and
    /// `pairing.json`.
    fn forget(&self, id: &ServiceId) -> Result<(), Error> {
        let _services = self.lock_services();
        self.forget_locked(id, true)
    }

    /// Deletes this machine's pairing with service `id` if its folder no
    /// longer has it (spec/machine.md §1.3): the token, once `pairing.json`
    /// or `service.json` is gone, and `pairing.json` too once
    /// `service.json` is. Returns whether it did. What it checks is read
    /// afresh while no pairing is being recorded, so a pairing made
    /// meanwhile stays.
    pub(crate) fn forget_if_gone(&self, id: &ServiceId) -> Result<bool, Error> {
        let _services = self.lock_services();
        let home = &self.inner.home;
        let absent = |path: &Path| match std::fs::symlink_metadata(path) {
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            Ok(_) => false,
        };
        let service_gone = absent(&home.service_file(id));
        if !service_gone && !absent(&home.pairing_file(id)) {
            return Ok(false);
        }
        self.forget_locked(id, service_gone)?;
        Ok(true)
    }

    /// Deletes the token of service `id`, and its `pairing.json` if
    /// `pairing_file` says to, while holding the services' lock.
    fn forget_locked(&self, id: &ServiceId, pairing_file: bool) -> Result<(), Error> {
        self.inner.secrets.delete(&self.token_key(id))?;
        let path = self.inner.home.pairing_file(id);
        if pairing_file {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(with_path(e, &path).into());
                }
                _ => {}
            }
        }
        self.prune_bundle_store();
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
    fn previous_version(&self, service: &Service, bundle: &Bundle) -> Option<Bundle> {
        service
            .approvals
            .iter()
            .rev()
            .filter(|a| a.name == bundle.manifest().name && a.hash != bundle.hash())
            .find_map(|a| match folders::check(&self.inner.home, &service.id, a) {
                Ok(approved) => Some(approved),
                Err(e) => {
                    tracing::warn!("the approved copy of {} is damaged: {e}", a.hash);
                    None
                }
            })
    }

    /// Approves exactly the reviewed bundle for the reviewed service: writes
    /// its three files, read-only, into the service's folder, and adds the
    /// approval to `service.json`. Approving a bundle again restores its
    /// files if they changed. Fails if the service's base URL changed
    /// since the review.
    pub fn approve(&self, review: &Review) -> Result<Service, Error> {
        let _services = self.lock_services();
        let id = &review.service.id;
        let mut file = self.read_service_file(id)?;
        if file.base_url != review.service.base_url {
            return Err(Error::ServiceMoved(id.clone()));
        }
        let bundle = &review.bundle;
        let manifest = bundle.manifest();
        let dir = self
            .inner
            .home
            .approved_bundle_dir(id, &manifest.name, &bundle.hash());
        folders::write_bundle(&dir, bundle).map_err(|e| with_path(e, &dir))?;
        let approval = Approval {
            hash: bundle.hash(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            approved_at: crate::rfc3339::now(),
        };
        let existing = file.approvals.iter_mut().find(|a| a.hash == approval.hash);
        let changed = match existing {
            Some(existing)
                if (&existing.name, &existing.version) == (&approval.name, &approval.version) =>
            {
                false
            }
            // It named the bundle wrongly, so it didn't count.
            Some(existing) => {
                *existing = approval;
                true
            }
            None => {
                file.approvals.push(approval);
                true
            }
        };
        if changed {
            self.save_service_file(id, &file)?;
        }
        self.service(id)
    }

    /// Withdraws an approval, whether it counts or not (spec/machine.md
    /// §3.2): removes it from `service.json`, and the bundle's files from
    /// the service's folder. The runtime's copy goes once no paired service
    /// approves the bundle. Fails if `service.json` lists no approval of
    /// `hash`.
    pub fn revoke(&self, id: &ServiceId, hash: BundleHash) -> Result<Service, Error> {
        let _services = self.lock_services();
        let mut file = self.read_service_file(id)?;
        let (revoked, kept) = std::mem::take(&mut file.approvals)
            .into_iter()
            .partition::<Vec<_>, _>(|a| a.hash == hash);
        if revoked.is_empty() {
            return Err(Error::NotApproved {
                service: id.clone(),
                hash,
            });
        }
        file.approvals = kept;
        self.save_service_file(id, &file)?;
        for approval in revoked.iter().filter(|a| is_bundle_name(&a.name)) {
            let dir = self
                .inner
                .home
                .approved_bundle_dir(id, &approval.name, &hash);
            folders::remove_bundle(&dir).map_err(|e| with_path(e, &dir))?;
            if let Some(name_dir) = dir.parent() {
                folders::remove_if_empty(name_dir);
            }
        }
        self.prune_bundle_store();
        self.service(id)
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
        net::check_url(url, self.allows_insecure_http())
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

    /// Service `id`, if this machine is paired with it, and its token.
    async fn credentials(&self, id: &ServiceId) -> Result<Credentials, Error> {
        let id = id.clone();
        self.blocking(move |machine| {
            let service = machine.service(&id)?;
            let not_paired = || Error::NotPaired(id.clone());
            let base_url = service
                .pairing
                .as_ref()
                .ok_or_else(not_paired)?
                .base_url
                .clone();
            let token = machine.token(&id)?.ok_or_else(not_paired)?;
            Ok(Credentials {
                service,
                base_url,
                token,
            })
        })
        .await
    }

    /// `GET`s a machine-authenticated endpoint of a service.
    async fn get(&self, to: &Credentials, path: &str) -> Result<reqwest::Response, Error> {
        self.check_url(&to.base_url)?;
        let url = net::endpoint(&to.base_url, path);
        self.net()
            .http
            .get(&url)
            .bearer_auth(&to.token)
            .send()
            .await
            .map_err(|e| net::unreachable(to.base_url.as_str(), &e))
    }

    /// A service's `service.json` as it is, approvals that don't count
    /// included.
    fn read_service_file(&self, id: &ServiceId) -> Result<ServiceFile, Error> {
        let path = self.inner.home.service_file(id);
        let file = read_strict_json::<ServiceFile>(&path)?
            .ok_or_else(|| Error::UnknownService(id.to_string()))?;
        file.check()
            .map_err(|reason| Error::Invalid { path, reason })?;
        Ok(file)
    }

    fn save_service_file(&self, id: &ServiceId, file: &ServiceFile) -> Result<(), Error> {
        write_json(&self.inner.home.service_file(id), file, PUBLIC)
    }

    /// The names in `services/`, lowercased: IDs that a new service can't
    /// take, even on a file system that ignores case.
    fn folder_names(&self) -> Result<HashSet<String>, Error> {
        let dir = self.inner.home.services_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
            Err(e) => return Err(with_path(e, &dir).into()),
        };
        let mut names = HashSet::new();
        for entry in entries {
            let name = entry.map_err(|e| with_path(e, &dir))?.file_name();
            names.insert(name.to_string_lossy().to_lowercase());
        }
        Ok(names)
    }

    /// Held while a service's files are read, changed and written back,
    /// so that whoever reads them while it's held sees none of moil's own
    /// changes half done.
    pub(crate) fn lock_services(&self) -> MutexGuard<'_, ()> {
        self.inner
            .services
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Removes from the runtime's store the bundles that no paired service
    /// approves, so that it holds only what may run; the runtime then
    /// removes their environments. Best effort: what it can't remove is
    /// logged, and left for next time.
    fn prune_bundle_store(&self) {
        let keep: HashSet<BundleHash> = match self.services() {
            Ok(services) => services
                .iter()
                .filter(|s| s.pairing.is_some())
                .flat_map(|s| s.approvals.iter().map(|a| a.hash))
                .collect(),
            Err(e) => {
                tracing::warn!("couldn't tell which stored bundles are still approved: {e}");
                return;
            }
        };
        let store = self.inner.home.bundle_store();
        let entries = match std::fs::read_dir(store.root()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!("couldn't look through {}: {e}", store.root().display());
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(hash) = name.to_str().and_then(|n| n.parse::<BundleHash>().ok()) else {
                continue;
            };
            if !keep.contains(&hash)
                && let Err(e) = store.remove(&hash)
            {
                tracing::warn!("couldn't remove {}: {e}", store.dir(&hash).display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemorySecrets;

    #[test]
    fn hello_stays_within_what_services_accept() {
        let dir = tempfile::tempdir().unwrap();
        let machine = Machine::open(MachineConfig {
            home: Home::new(dir.path()),
            secrets: Arc::new(MemorySecrets::default()),
            app_version: format!("0.1.0+{}", "build".repeat(20)),
        })
        .unwrap();
        let gpu = |i| moil_proto::Gpu {
            name: format!("GPU {i}"),
            memory_bytes: None,
        };
        let hardware = Hardware {
            computer_name: format!("  Ada's {}  ", "very ".repeat(20)),
            cpus: 256,
            memory_bytes: 1 << 40,
            gpus: (0..100).map(gpu).collect(),
        };
        machine.inner.hardware.set(hardware).unwrap();

        let info = machine.info();
        assert_eq!(info.name, format!("Ada's {}", "very ".repeat(20))[..64]);
        assert_eq!(info.gpus, (0..64).map(gpu).collect::<Vec<_>>());
        assert_eq!(machine.app_version().chars().count(), 64);
    }
}
