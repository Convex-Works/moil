//! `moil-app --demo`: the app on a simulated agent, to try every screen
//! without a service.
//!
//! It simulates two services: klisi, connected, which offers transcribe
//! 0.3.0 to replace the approved 0.2.0 (so the review shows a diff), and
//! lab, which keeps failing to connect. Jobs for klisi come and go through
//! every phase; pairing with any address succeeds after a few seconds with
//! the code WDJB-MJHT, unless its host starts with `deny`.
//!
//! The transcribe bundles are real: their lockfiles come from
//! `uv lock --script`, and their assets' hashes from Hugging Face.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures::FutureExt;
use futures::future::BoxFuture;
use moil_agent::{
    Approval, BundleStatus, Connection, Error, JobStatus, PairedService, Review, ServiceId,
    ServiceStatus, Snapshot,
};
use moil_bundle::Bundle;
use moil_proto::{BundleHash, BundleInfo, JobId, Phase};
use tokio::sync::watch;
use tokio::time::sleep;
use url::Url;

use crate::backend::{Backend, Pairing, PairingCode, Result};
use crate::links::service_address;

/// transcribe 0.2.0, the version the demo's owner approved weeks ago.
pub fn transcribe_0_2() -> Bundle {
    Bundle::new(
        include_str!("../demo/transcribe-0.2.0/job.py"),
        include_str!("../demo/transcribe-0.2.0/job.py.lock"),
        include_str!("../demo/transcribe-0.2.0/manifest.json"),
    )
    .expect("the demo bundle is valid")
}

/// transcribe 0.3.0, which adds speaker labels and takes faster-whisper from
/// git, waiting for review.
pub fn transcribe_0_3() -> Bundle {
    Bundle::new(
        include_str!("../demo/transcribe-0.3.0/job.py"),
        include_str!("../demo/transcribe-0.3.0/job.py.lock"),
        include_str!("../demo/transcribe-0.3.0/manifest.json"),
    )
    .expect("the demo bundle is valid")
}

/// The smallest bundle from the spec's vectors.
fn minimal() -> Bundle {
    Bundle::new(
        include_str!("../../../spec/vectors/bundles/minimal/job.py"),
        include_str!("../../../spec/vectors/bundles/minimal/job.py.lock"),
        include_str!("../../../spec/vectors/bundles/minimal/manifest.json"),
    )
    .expect("the spec's minimal bundle is valid")
}

/// How long the demo's simulated work takes.
mod pace {
    use std::time::Duration;

    pub const IDLE: Duration = Duration::from_secs(5);
    pub const PREPARING: Duration = Duration::from_secs(2);
    pub const DOWNLOADING: Duration = Duration::from_millis(1500);
    /// Between progress reports: 100 of them make a 25-second job.
    pub const PROGRESS: Duration = Duration::from_millis(250);
    pub const UPLOADING: Duration = Duration::from_millis(1500);
    pub const FETCH: Duration = Duration::from_millis(600);
    pub const CONTACT: Duration = Duration::from_millis(800);
    pub const CONFIRM: Duration = Duration::from_secs(5);
    pub const CONNECT: Duration = Duration::from_millis(1500);
    pub const FIRST_RETRY: Duration = Duration::from_secs(4);
    pub const MAX_RETRY: Duration = Duration::from_secs(30);
    pub const SHUTDOWN: Duration = Duration::from_millis(400);
}

const MEETINGS: [&str; 3] = ["Weekly sync", "Design review", "1:1 Ada / Grace"];

/// A simulated agent. See the module docs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Demo(Arc<State>);

#[derive(Debug)]
struct State {
    snapshot: watch::Sender<Snapshot>,
    /// What the demo's owner paired with and approved.
    paired: Mutex<Vec<PairedService>>,
    /// Set to stop the running job.
    stop: watch::Sender<bool>,
    bundles: Vec<Bundle>,
}

impl Demo {
    /// Starts the simulation. Must be called within a Tokio runtime.
    pub fn start() -> Self {
        let now = SystemTime::now();
        let days_ago = |days: u64| now - Duration::from_secs(days * 86_400);
        let (v2, v3, minimal) = (transcribe_0_2(), transcribe_0_3(), minimal());
        let approval = |bundle: &Bundle, days| Approval {
            hash: bundle.hash(),
            name: bundle.manifest().name.clone(),
            version: bundle.manifest().version.clone(),
            approved_at: days_ago(days),
        };
        let klisi_url: Url = "https://klisi.example.com/moil".parse().expect("a URL");
        let klisi = PairedService {
            id: ServiceId::derive(&klisi_url, |_| false),
            base_url: klisi_url,
            name: "klisi".into(),
            machine_id: "m_7Hq2Xk9Lw3".into(),
            paired_at: days_ago(64),
            approvals: vec![approval(&v2, 23)],
        };
        let lab_url: Url = "https://lab.example.org/moil".parse().expect("a URL");
        let lab = PairedService {
            id: ServiceId::derive(&lab_url, |_| false),
            base_url: lab_url,
            name: "lab".into(),
            machine_id: "m_Qe2wBv0V8i".into(),
            paired_at: days_ago(9),
            approvals: vec![approval(&minimal, 9)],
        };
        let snapshot = Snapshot {
            paused: false,
            services: vec![
                status(&klisi, Connection::Connected, &[&v2, &v3]),
                status(
                    &lab,
                    Connection::Disconnected {
                        error: "connection refused".into(),
                        retry_at: Instant::now() + pace::FIRST_RETRY,
                    },
                    &[&minimal],
                ),
            ],
            job: None,
            last_job: None,
        };
        let (klisi_id, lab_id) = (klisi.id.clone(), lab.id.clone());
        let state = Arc::new(State {
            snapshot: watch::Sender::new(snapshot),
            paired: Mutex::new(vec![klisi, lab]),
            stop: watch::Sender::new(false),
            bundles: vec![v2, v3, minimal],
        });
        tokio::spawn(state.clone().run_jobs(klisi_id));
        tokio::spawn(state.clone().keep_reconnecting(lab_id));
        Self(state)
    }
}

impl State {
    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        self.snapshot.send_modify(change);
    }

    fn update_job(&self, change: impl FnOnce(&mut JobStatus)) {
        self.update(|s| {
            if let Some(job) = &mut s.job {
                change(job);
            }
        });
    }

    /// Waits for `duration`, or less if the job is stopped. Returns whether
    /// the job should carry on.
    async fn work(&self, duration: Duration) -> bool {
        let mut stop = self.stop.subscribe();
        tokio::select! {
            _ = sleep(duration) => true,
            _ = stop.wait_for(|stop| *stop) => false,
        }
    }

    /// Runs a job for `klisi` now and then, while not paused.
    async fn run_jobs(self: Arc<Self>, klisi: ServiceId) {
        let mut paused = self.snapshot.subscribe();
        for (n, meeting) in MEETINGS.iter().cycle().enumerate() {
            sleep(pace::IDLE).await;
            if paused.wait_for(|s| !s.paused).await.is_err() {
                return;
            }
            let Some(bundle) = self.approved_bundles(&klisi).pop() else {
                continue;
            };
            self.stop.send_replace(false);
            self.update(|s| {
                s.job = Some(JobStatus {
                    service: klisi.clone(),
                    job_id: JobId::new(format!("j_{}", 4100 + n)).expect("a job ID"),
                    attempt: 1,
                    title: Some((*meeting).into()),
                    bundle_name: bundle.manifest().name.clone(),
                    phase: Phase::Preparing,
                    fraction: None,
                    message: None,
                    started_at: Instant::now(),
                    stopping: false,
                })
            });
            self.run_job().await;
            self.update(|s| s.job = None);
        }
    }

    async fn run_job(&self) {
        if !self.work(pace::PREPARING).await {
            return;
        }
        self.update_job(|job| job.phase = Phase::Downloading);
        if !self.work(pace::DOWNLOADING).await {
            return;
        }
        self.update_job(|job| job.phase = Phase::Running);
        for percent in 0..=100u32 {
            let message = match percent {
                0..5 => "loading model",
                5..60 => "transcribing",
                60..95 => "diarizing",
                _ => "writing transcript",
            };
            self.update_job(|job| {
                job.fraction = Some(f64::from(percent) / 100.0);
                job.message = Some(message.into());
            });
            if !self.work(pace::PROGRESS).await {
                return;
            }
        }
        self.update_job(|job| job.phase = Phase::Uploading);
        self.work(pace::UPLOADING).await;
    }

    /// Fails to connect to `service` forever, backing off like the agent.
    async fn keep_reconnecting(self: Arc<Self>, service: ServiceId) {
        let mut backoff = pace::FIRST_RETRY;
        loop {
            sleep(backoff).await;
            if !self.set_connection(&service, Connection::Connecting) {
                return;
            }
            sleep(pace::CONNECT).await;
            backoff = (backoff * 2).min(pace::MAX_RETRY);
            let refused = Connection::Disconnected {
                error: "connection refused".into(),
                retry_at: Instant::now() + backoff,
            };
            if !self.set_connection(&service, refused) {
                return;
            }
        }
    }

    /// Returns false once the service is gone.
    fn set_connection(&self, id: &ServiceId, connection: Connection) -> bool {
        let mut found = false;
        self.update(|s| {
            if let Some(service) = s.services.iter_mut().find(|s| s.id == *id) {
                service.connection = connection;
                found = true;
            }
        });
        found
    }

    fn service(&self, id: &ServiceId) -> Result<PairedService> {
        let paired = self.paired.lock().expect("the demo's state");
        paired
            .iter()
            .find(|s| s.id == *id)
            .cloned()
            .ok_or_else(|| Error::UnknownService(id.to_string()))
    }

    fn bundle(&self, hash: BundleHash) -> Option<Bundle> {
        self.bundles.iter().find(|b| b.hash() == hash).cloned()
    }

    /// The bundles approved for a service, oldest approval first.
    fn approved_bundles(&self, id: &ServiceId) -> Vec<Bundle> {
        let Ok(service) = self.service(id) else {
            return Vec::new();
        };
        service
            .approvals
            .iter()
            .filter_map(|a| self.bundle(a.hash))
            .collect()
    }

    fn set_approved(&self, id: &ServiceId, hash: BundleHash, approved: bool) {
        self.update(|s| {
            let bundles = s.services.iter_mut().filter(|s| s.id == *id);
            for bundle in bundles.flat_map(|s| &mut s.bundles) {
                if bundle.info.hash == hash {
                    bundle.approved = approved;
                }
            }
        });
    }

    fn add_service(&self, url: &Url, name: &str) -> PairedService {
        let mut paired = self.paired.lock().expect("the demo's state");
        let service = PairedService {
            id: ServiceId::derive(url, |id| paired.iter().any(|s| s.id.as_str() == id)),
            base_url: url.clone(),
            name: name.to_owned(),
            machine_id: "m_demo".into(),
            paired_at: SystemTime::now(),
            approvals: Vec::new(),
        };
        paired.push(service.clone());
        self.update(|s| {
            s.services
                .push(status(&service, Connection::Connected, &[]))
        });
        service
    }

    fn review(&self, id: &ServiceId, hash: BundleHash) -> Result<Review> {
        let service = self.service(id)?;
        let bundle = self.bundle(hash).ok_or(Error::UnknownBundle(hash))?;
        let previous = service
            .approvals
            .iter()
            .rev()
            .filter(|a| a.name == bundle.manifest().name && a.hash != hash)
            .find_map(|a| self.bundle(a.hash));
        Ok(Review {
            approved: service.is_approved(&hash),
            service,
            bundle,
            previous,
        })
    }

    fn approve(&self, review: &Review) -> Result<()> {
        let (id, hash) = (&review.service.id, review.bundle.hash());
        self.change_service(id, |service| {
            if !service.is_approved(&hash) {
                service.approvals.push(Approval {
                    hash,
                    name: review.bundle.manifest().name.clone(),
                    version: review.bundle.manifest().version.clone(),
                    approved_at: SystemTime::now(),
                });
            }
        })?;
        self.set_approved(id, hash, true);
        Ok(())
    }

    fn revoke(&self, id: &ServiceId, hash: BundleHash) -> Result<()> {
        self.change_service(id, |service| service.approvals.retain(|a| a.hash != hash))?;
        self.set_approved(id, hash, false);
        Ok(())
    }

    fn unpair(&self, id: &ServiceId) {
        self.paired
            .lock()
            .expect("the demo's state")
            .retain(|s| s.id != *id);
        let running_for_it = self
            .snapshot
            .borrow()
            .job
            .as_ref()
            .is_some_and(|job| job.service == *id);
        if running_for_it {
            self.stop.send_replace(true);
        }
        self.update(|s| s.services.retain(|s| s.id != *id));
    }

    fn change_service(
        &self,
        id: &ServiceId,
        change: impl FnOnce(&mut PairedService),
    ) -> Result<()> {
        let mut paired = self.paired.lock().expect("the demo's state");
        let service = paired
            .iter_mut()
            .find(|s| s.id == *id)
            .ok_or_else(|| Error::UnknownService(id.to_string()))?;
        change(service);
        Ok(())
    }
}

/// A service as the snapshot shows it, offering `bundles`.
fn status(service: &PairedService, connection: Connection, bundles: &[&Bundle]) -> ServiceStatus {
    ServiceStatus {
        id: service.id.clone(),
        name: service.name.clone(),
        base_url: service.base_url.clone(),
        connection,
        bundles: bundles
            .iter()
            .map(|bundle| BundleStatus {
                info: BundleInfo {
                    hash: bundle.hash(),
                    name: bundle.manifest().name.clone(),
                    version: bundle.manifest().version.clone(),
                    description: bundle.manifest().description.clone(),
                },
                approved: service.is_approved(&bundle.hash()),
            })
            .collect(),
        insecure: service.base_url.scheme() == "http",
    }
}

impl Backend for Demo {
    fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.0.snapshot.subscribe()
    }

    fn pair(&self, base_url: String) -> BoxFuture<'_, Result<Pairing>> {
        async move {
            let url = service_address(&base_url).map_err(|reason| Error::NotAService {
                url: base_url.clone(),
                reason,
            })?;
            let host = url.host_str().unwrap_or_default().to_owned();
            let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]");
            if url.scheme() == "http" && !loopback {
                return Err(Error::InsecureUrl(url.to_string()));
            }
            sleep(pace::CONTACT).await;
            if host.ends_with(".invalid") {
                return Err(Error::NotAService {
                    url: url.to_string(),
                    reason: "GET /v1/info answered 404 Not Found".into(),
                });
            }
            let name = host.split('.').next().unwrap_or("service").to_owned();
            let origin = url.origin().ascii_serialization();
            let code = PairingCode {
                user_code: "WDJB-MJHT".into(),
                verification_uri: format!("{origin}/machines/pair"),
                verification_uri_complete: format!("{origin}/machines/pair?code=WDJB-MJHT"),
                service_name: name.clone(),
                expires_at: Instant::now() + Duration::from_secs(600),
            };
            let state = self.0.clone();
            let confirmed = async move {
                sleep(pace::CONFIRM).await;
                if host.starts_with("deny") {
                    return Err(Error::PairingDenied);
                }
                Ok(state.add_service(&url, &name))
            }
            .boxed();
            Ok(Pairing { code, confirmed })
        }
        .boxed()
    }

    fn review(&self, service: ServiceId, hash: BundleHash) -> BoxFuture<'_, Result<Review>> {
        async move {
            sleep(pace::FETCH).await;
            self.0.review(&service, hash)
        }
        .boxed()
    }

    fn approve(&self, review: Review) -> BoxFuture<'_, Result<()>> {
        async move {
            sleep(pace::FETCH).await;
            self.0.approve(&review)
        }
        .boxed()
    }

    fn revoke(&self, service: ServiceId, hash: BundleHash) -> BoxFuture<'_, Result<()>> {
        async move { self.0.revoke(&service, hash) }.boxed()
    }

    fn unpair(&self, service: ServiceId) -> BoxFuture<'_, Result<()>> {
        async move {
            sleep(pace::FETCH).await;
            self.0.unpair(&service);
            Ok(())
        }
        .boxed()
    }

    fn set_paused(&self, paused: bool) {
        self.0.update(|s| s.paused = paused);
    }

    fn stop_job(&self) {
        self.0.stop.send_replace(true);
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        async move {
            self.stop_job();
            sleep(pace::SHUTDOWN).await;
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(demo: &Demo) -> Snapshot {
        demo.subscribe().borrow().clone()
    }

    #[tokio::test(start_paused = true)]
    async fn starts_with_an_update_to_review_and_a_service_offline() {
        let demo = Demo::start();
        let snapshot = snapshot(&demo);
        let [klisi, lab] = &snapshot.services[..] else {
            panic!("two services: {snapshot:?}");
        };
        assert_eq!(klisi.connection, Connection::Connected);
        assert!(matches!(lab.connection, Connection::Disconnected { .. }));
        let pending: Vec<_> = snapshot
            .pending_reviews()
            .map(|(s, b)| (s.name.as_str(), b.info.hash))
            .collect();
        assert_eq!(pending, [("klisi", transcribe_0_3().hash())]);

        let review = demo
            .review(klisi.id.clone(), transcribe_0_3().hash())
            .await
            .unwrap();
        assert!(!review.approved);
        assert_eq!(
            review.previous.as_ref().map(Bundle::hash),
            Some(transcribe_0_2().hash())
        );
        assert!(
            review.diff().unwrap().iter().all(|d| !d.is_unchanged()),
            "every file changed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn jobs_go_through_every_phase() {
        let demo = Demo::start();
        let mut snapshots = demo.subscribe();
        let (mut phases, mut fractions) = (Vec::new(), Vec::new());
        loop {
            snapshots.changed().await.unwrap();
            let Some(job) = snapshots.borrow_and_update().job.clone() else {
                if phases.is_empty() {
                    continue;
                }
                break;
            };
            if phases.last() != Some(&job.phase) {
                phases.push(job.phase);
            }
            fractions.extend(job.fraction.filter(|_| job.phase == Phase::Running));
        }
        assert_eq!(
            phases,
            [
                Phase::Preparing,
                Phase::Downloading,
                Phase::Running,
                Phase::Uploading
            ]
        );
        assert!(fractions.is_sorted() && fractions.first() == Some(&0.0));
        assert_eq!(fractions.last(), Some(&1.0));
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_ends_the_job_and_pausing_holds_off_the_next() {
        let demo = Demo::start();
        let mut snapshots = demo.subscribe();
        snapshots.wait_for(|s| s.job.is_some()).await.unwrap();
        demo.stop_job();
        snapshots.wait_for(|s| s.job.is_none()).await.unwrap();

        demo.set_paused(true);
        sleep(Duration::from_secs(600)).await;
        assert!(snapshot(&demo).job.is_none(), "no job while paused");

        demo.set_paused(false);
        snapshots.wait_for(|s| s.job.is_some()).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn pairing_shows_a_code_then_adds_the_service() {
        let demo = Demo::start();
        let pairing = demo.pair("notes.example.net/moil".into()).await.unwrap();
        assert_eq!(pairing.code.user_code, "WDJB-MJHT");
        assert_eq!(
            pairing.code.verification_uri_complete,
            "https://notes.example.net/machines/pair?code=WDJB-MJHT"
        );
        let service = pairing.confirmed.await.unwrap();
        assert_eq!(service.name, "notes");
        assert!(
            snapshot(&demo)
                .services
                .iter()
                .any(|s| s.id == service.id && s.connection == Connection::Connected)
        );

        let loopback = demo.pair("http://[::1]:8787/moil".into()).await.unwrap();
        let service = loopback.confirmed.await.unwrap();
        assert_eq!(
            service.id.as_str(),
            "1-8787",
            "IDs are derived as the agent does"
        );

        let denied = demo.pair("https://deny.example.net/moil".into()).await;
        assert!(matches!(
            denied.unwrap().confirmed.await,
            Err(Error::PairingDenied)
        ));
        assert!(matches!(
            demo.pair("http://klisi.example.com/moil".into()).await,
            Err(Error::InsecureUrl(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn approving_the_update_settles_the_review() {
        let demo = Demo::start();
        let klisi: ServiceId = "klisi.example.com".parse().unwrap();
        let review = demo
            .review(klisi.clone(), transcribe_0_3().hash())
            .await
            .unwrap();
        demo.approve(review).await.unwrap();
        assert_eq!(snapshot(&demo).pending_reviews().count(), 0);
        let again = demo
            .review(klisi.clone(), transcribe_0_3().hash())
            .await
            .unwrap();
        assert!(again.approved);

        demo.revoke(klisi, transcribe_0_3().hash()).await.unwrap();
        assert_eq!(snapshot(&demo).pending_reviews().count(), 1);
    }
}
