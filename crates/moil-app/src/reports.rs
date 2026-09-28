//! What the tray shows of the home besides the agent's snapshot: each
//! service's hooks and what's wrong with the home, as `moil status` reports
//! them (spec/machine.md §3.3).

use std::collections::BTreeMap;
use std::time::Duration;

use futures::future::BoxFuture;
use moil_agent::report::{HookReport, Problem, Status};
use moil_agent::{Error, ServiceId};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;

/// How often, at most, the report is built. A running job changes the
/// agent's state many times a second, and a report reads every service's
/// folder.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// The home's hooks and problems, from its status report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeReport {
    /// Each service's hooks, by service ID.
    pub hooks: BTreeMap<ServiceId, Vec<HookReport>>,
    /// What's wrong with the home, for the owner to fix.
    pub problems: Vec<Problem>,
}

impl HomeReport {
    pub fn new(status: &Status) -> Self {
        Self {
            hooks: status
                .services
                .iter()
                .filter(|s| !s.hooks.is_empty())
                .map(|s| (s.id.clone(), s.hooks.clone()))
                .collect(),
            problems: status.problems.clone(),
        }
    }

    /// The hooks of the service `id`.
    pub fn hooks(&self, id: &ServiceId) -> &[HookReport] {
        self.hooks.get(id).map_or(&[], Vec::as_slice)
    }
}

/// Keeps a [`HomeReport`] current on `runtime`: builds it from `status`
/// now, then again whenever `changes` shows the agent's state changed, at
/// most once every [`MIN_INTERVAL`]. The receiver sees a change only when
/// the report differs from the one before. Building stops once the
/// receiver is dropped or the agent is gone.
pub fn watch<F>(
    mut changes: watch::Receiver<()>,
    runtime: &Handle,
    status: F,
) -> watch::Receiver<HomeReport>
where
    F: Fn() -> BoxFuture<'static, Result<Status, Error>> + Send + 'static,
{
    let (reports, receiver) = watch::channel(HomeReport::default());
    runtime.spawn(async move {
        let mut failure = None;
        loop {
            changes.borrow_and_update();
            let next = Instant::now() + MIN_INTERVAL;
            match status().await {
                Ok(status) => {
                    failure = None;
                    let report = HomeReport::new(&status);
                    reports.send_if_modified(|shown| {
                        let changed = *shown != report;
                        *shown = report;
                        changed
                    });
                }
                Err(e) => {
                    // Once for each new reason, however often it is asked.
                    let reason = e.to_string();
                    if failure.as_ref() != Some(&reason) {
                        tracing::warn!("couldn't read the home's hooks and problems: {reason}");
                        failure = Some(reason);
                    }
                }
            }
            tokio::select! {
                changed = changes.changed() => if changed.is_err() {
                    return;
                },
                () = reports.closed() => return,
            }
            tokio::time::sleep_until(next).await;
        }
    });
    receiver
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::FutureExt;
    use moil_agent::report::ServiceReport;
    use tokio::sync::mpsc;

    use super::*;

    fn klisi() -> ServiceId {
        "klisi.example.com".parse().unwrap()
    }

    fn hook(bundle: &str, runnable: bool) -> HookReport {
        let service = "/h/services/klisi.example.com";
        HookReport {
            bundle: bundle.into(),
            path: format!("{service}/hooks/{bundle}").into(),
            log: format!("{service}/logs/{bundle}.log").into(),
            runnable,
        }
    }

    fn status(hooks: Vec<HookReport>, problems: Vec<Problem>) -> Status {
        Status {
            home: "/h".into(),
            agent: None,
            paused: false,
            job: None,
            last_job: None,
            services: vec![ServiceReport {
                id: klisi(),
                name: "klisi".into(),
                base_url: "https://klisi.example.com/moil".parse().unwrap(),
                paired: true,
                connection: None,
                approvals: Vec::new(),
                offered: None,
                hooks,
            }],
            problems,
        }
    }

    /// A home whose status the test changes, and that says when a report
    /// is built from it.
    struct FakeHome {
        status: Arc<Mutex<Result<Status, String>>>,
        built: mpsc::UnboundedReceiver<Instant>,
    }

    impl FakeHome {
        fn watch(
            status: Status,
            changes: watch::Receiver<()>,
        ) -> (Self, watch::Receiver<HomeReport>) {
            let status = Arc::new(Mutex::new(Ok(status)));
            let (built, built_at) = mpsc::unbounded_channel();
            let shared = status.clone();
            let reports = watch(changes, &Handle::current(), move || {
                let _ = built.send(Instant::now());
                let status = shared.lock().unwrap().clone();
                async move { status.map_err(Error::Config) }.boxed()
            });
            let home = Self {
                status,
                built: built_at,
            };
            (home, reports)
        }

        fn set(&self, status: Result<Status, String>) {
            *self.status.lock().unwrap() = status;
        }

        async fn next_build(&mut self) -> Instant {
            self.built.recv().await.expect("the watcher runs")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_report_follows_the_home_as_the_agent_changes() {
        let (agent, changes) = watch::channel(());
        let problem = Problem {
            path: "services/klisi.example.com/hooks/summarize".into(),
            message: "moil can't run this hook: it isn't executable. Run chmod +x on it.".into(),
        };
        let (mut home, mut reports) =
            FakeHome::watch(status(vec![hook("transcribe", true)], Vec::new()), changes);
        home.next_build().await;
        reports.changed().await.unwrap();
        assert_eq!(
            reports.borrow_and_update().hooks(&klisi()),
            [hook("transcribe", true)]
        );

        home.set(Ok(status(
            vec![hook("summarize", false), hook("transcribe", true)],
            vec![problem.clone()],
        )));
        agent.send_replace(());
        reports.changed().await.unwrap();
        let report = reports.borrow_and_update().clone();
        assert_eq!(report.hooks(&klisi()).len(), 2);
        assert_eq!(report.problems, [problem]);
        assert!(report.hooks(&"lab.example.org".parse().unwrap()).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_different_report_is_shown() {
        let (agent, changes) = watch::channel(());
        let (mut home, mut reports) =
            FakeHome::watch(status(vec![hook("transcribe", true)], Vec::new()), changes);
        home.next_build().await;
        reports.changed().await.unwrap();
        reports.borrow_and_update();

        // The agent's state changes, but not the hooks.
        agent.send_replace(());
        home.next_build().await;
        tokio::task::yield_now().await;
        assert!(!reports.has_changed().unwrap());

        // Failing to read the home keeps what was shown.
        home.set(Err("services/ is unreadable".into()));
        agent.send_replace(());
        home.next_build().await;
        tokio::task::yield_now().await;
        assert!(!reports.has_changed().unwrap());
        assert_eq!(reports.borrow().hooks(&klisi()).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_changes_builds_a_report_once_a_second() {
        let (agent, changes) = watch::channel(());
        let (mut home, _reports) = FakeHome::watch(status(Vec::new(), Vec::new()), changes);
        let first = home.next_build().await;

        // A job reports progress every 10 ms for 3 seconds.
        let progress = tokio::spawn(async move {
            for _ in 0..300 {
                agent.send_replace(());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            agent
        });
        let mut builds = vec![first];
        while builds.len() < 4 {
            builds.push(home.next_build().await);
        }
        for pair in builds.windows(2) {
            assert!(pair[1] - pair[0] >= MIN_INTERVAL, "{builds:?}");
        }
        let agent = progress.await.unwrap();

        drop(agent);
        let stopped = tokio::time::timeout(Duration::from_secs(60), async {
            while home.built.recv().await.is_some() {}
        });
        assert!(stopped.await.is_ok(), "the watcher stops with the agent");
    }
}
