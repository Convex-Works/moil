//! `moil status` (spec/machine.md §3.3), built from the home and, while an
//! agent runs on it, what the agent's [`Snapshot`] says. Whether or not an
//! agent runs, the report is built here, so that whatever it gains is added
//! in one place.

use std::time::{Duration, Instant, SystemTime};

use crate::report::{
    AgentInfo, ConnectionReport, EndedJob, ErrorReport, OfferedBundle, Outcome, RunningJob,
    ServiceReport, Status,
};
use crate::{
    AgentHandle, Connection, Error, Home, JobOutcome, JobReport, JobStatus, ReadOnlyMachine,
    Service, ServiceStatus, Snapshot,
};

/// The status of a home no agent runs on: what its files say.
pub fn from_home(machine: &ReadOnlyMachine) -> Result<Status, Error> {
    let home = machine.home();
    let paused = home.load_settings()?.paused;
    Ok(build(home, &machine.services()?, paused, None))
}

/// The status of the home `agent` runs on, as `info` describes the process
/// running it.
pub async fn from_agent(agent: &AgentHandle, info: &AgentInfo) -> Result<Status, Error> {
    let services = agent
        .machine()
        .blocking(|machine| machine.services())
        .await?;
    let snapshot = agent.snapshot();
    let home = agent.machine().home();
    Ok(build(
        home,
        &services,
        snapshot.paused,
        Some((info, &snapshot)),
    ))
}

fn build(
    home: &Home,
    services: &[Service],
    paused: bool,
    agent: Option<(&AgentInfo, &Snapshot)>,
) -> Status {
    let clock = Clock::now();
    let snapshot = agent.map(|(_, snapshot)| snapshot);
    Status {
        home: std::path::absolute(home.root()).unwrap_or_else(|_| home.root().to_owned()),
        agent: agent.map(|(info, _)| info.clone()),
        paused,
        job: snapshot
            .and_then(|s| s.job.as_ref())
            .map(|job| running_job(job, &clock)),
        last_job: snapshot.and_then(|s| s.last_job.as_ref()).map(ended_job),
        services: services
            .iter()
            .map(|service| {
                let status = snapshot.and_then(|s| s.service(&service.id));
                service_report(service, status, &clock)
            })
            .collect(),
        problems: Vec::new(),
    }
}

fn service_report(
    service: &Service,
    status: Option<&ServiceStatus>,
    clock: &Clock,
) -> ServiceReport {
    ServiceReport {
        id: service.id.clone(),
        name: service.name.clone(),
        base_url: service.base_url.clone(),
        paired: service.pairing.is_some(),
        connection: status.map(|s| connection_report(&s.connection, clock)),
        approvals: service.approvals.clone(),
        offered: status.map(|s| {
            s.bundles
                .iter()
                .map(|b| OfferedBundle {
                    hash: b.info.hash,
                    name: b.info.name.clone(),
                    version: b.info.version.clone(),
                    description: b.info.description.clone(),
                    approved: b.approved,
                })
                .collect()
        }),
        hooks: Vec::new(),
    }
}

fn connection_report(connection: &Connection, clock: &Clock) -> ConnectionReport {
    match connection {
        Connection::Connecting => ConnectionReport::Connecting,
        Connection::Connected => ConnectionReport::Connected,
        Connection::Disconnected { error, retry_at } => ConnectionReport::Offline {
            error: error.clone(),
            retry_at: clock.wall(*retry_at),
        },
        Connection::Unpaired => ConnectionReport::Unpaired,
        Connection::Replaced => ConnectionReport::Replaced,
    }
}

/// The running attempt, as reports show it.
pub(crate) fn running_job(job: &JobStatus, clock: &Clock) -> RunningJob {
    RunningJob {
        service: job.service.clone(),
        job_id: job.job_id.clone(),
        attempt: job.attempt,
        title: job.title.clone(),
        bundle: job.bundle_name.clone(),
        phase: job.phase,
        fraction: job.fraction,
        message: job.message.clone(),
        started_at: clock.wall(job.started_at),
        stopping: job.stopping,
    }
}

fn ended_job(job: &JobReport) -> EndedJob {
    let (outcome, error) = match &job.outcome {
        JobOutcome::Succeeded => (Outcome::Succeeded, None),
        JobOutcome::Failed { code, message } => (
            Outcome::Failed,
            Some(ErrorReport {
                code: *code,
                message: message.clone(),
            }),
        ),
        JobOutcome::Cancelled => (Outcome::Cancelled, None),
        JobOutcome::Interrupted => (Outcome::Interrupted, None),
    };
    EndedJob {
        service: job.service.clone(),
        job_id: job.job_id.clone(),
        attempt: job.attempt,
        title: job.title.clone(),
        bundle: job.bundle_name.clone(),
        outcome,
        error,
        ended_at: job.ended_at,
    }
}

/// Places the agent's [`Instant`]s on the wall clock, as of when it was
/// made.
pub(crate) struct Clock {
    instant: Instant,
    wall: SystemTime,
}

impl Clock {
    pub(crate) fn now() -> Self {
        Self {
            instant: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    /// When `instant` is, or was, on the wall clock, to the second: as
    /// precise as reports show times, so that reports built moments apart
    /// agree on it.
    pub(crate) fn wall(&self, instant: Instant) -> SystemTime {
        let wall = match instant.checked_duration_since(self.instant) {
            Some(ahead) => self.wall.checked_add(ahead),
            None => self.wall.checked_sub(self.instant - instant),
        }
        .unwrap_or(self.wall);
        let since_epoch = wall
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        SystemTime::UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants_land_on_the_wall_clock_to_the_second() {
        let earlier = Instant::now();
        let clock = Clock {
            instant: earlier + Duration::from_secs(90),
            wall: SystemTime::UNIX_EPOCH + Duration::from_millis(1_790_000_000_750),
        };
        let second = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(clock.wall(clock.instant), second(1_790_000_000));
        let later = clock.instant + Duration::from_millis(300);
        assert_eq!(clock.wall(later), second(1_790_000_001));
        assert_eq!(clock.wall(earlier), second(1_789_999_910));
    }
}
