//! What can cut an attempt short: the stop signal and the time limit
//! (rules 1–3 of spec §8.5).

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use crate::outcome::Cut;
use crate::{StopReason, StopSignal};

/// The stop signal and the time limit of one attempt.
#[derive(Debug)]
pub(crate) struct Limits {
    stop: StopSignal,
    timeout: Option<Duration>,
    deadline: Option<Instant>,
}

impl Limits {
    /// Limits whose time runs from now.
    pub fn new(stop: StopSignal, timeout: Option<Duration>) -> Self {
        Self {
            stop,
            timeout,
            deadline: timeout.map(|limit| Instant::now() + limit),
        }
    }

    /// Resolves once the attempt must end early. Cancel-safe.
    pub async fn reached(&mut self) -> Cut {
        let (deadline, timeout) = (self.deadline, self.timeout);
        let expired = async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let cut = tokio::select! {
            biased;
            reason = self.stop.stopped() => Cut::Stopped(reason),
            () = expired => Cut::TimedOut(timeout.unwrap_or_default()),
        };
        self.decide(Some(cut)).unwrap_or(cut)
    }

    /// Why the attempt is being stopped, if it is. Rules 1 and 2 come
    /// first: a stop wins over however else the attempt ends, whenever it
    /// came.
    pub fn stopped(&self) -> Option<StopReason> {
        self.stop.reason()
    }

    /// `cut`, unless a stop overrules it: a stop wins over running out of
    /// time, which wins over everything else.
    pub fn decide(&self, cut: Option<Cut>) -> Option<Cut> {
        self.stopped().map(Cut::Stopped).or(cut)
    }

    /// Runs `work` unless the attempt must end first, in which case `work`
    /// is dropped.
    pub async fn guard<T>(&mut self, work: impl Future<Output = T>) -> Result<T, Cut> {
        tokio::select! {
            biased;
            cut = self.reached() => Err(cut),
            value = work => Ok(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stop_signal;

    const LIMIT: Duration = Duration::from_secs(60);

    #[tokio::test(start_paused = true)]
    async fn work_is_cut_short_when_time_runs_out() {
        let (_stopper, stop) = stop_signal();
        let mut limits = Limits::new(stop, Some(LIMIT));
        let started = Instant::now();
        let cut = limits.guard(std::future::pending::<()>()).await;
        assert_eq!(cut, Err(Cut::TimedOut(LIMIT)));
        assert_eq!(started.elapsed(), LIMIT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_after_time_ran_out_wins() {
        // As when the service cancels while a timed-out script cleans up.
        let (stopper, stop) = stop_signal();
        let mut limits = Limits::new(stop, Some(LIMIT));
        let cut = limits.reached().await;
        assert_eq!(cut, Cut::TimedOut(LIMIT));

        stopper.stop(StopReason::Cancelled);
        assert_eq!(
            limits.decide(Some(cut)),
            Some(Cut::Stopped(StopReason::Cancelled))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_wins_when_time_runs_out_at_once() {
        let (stopper, stop) = stop_signal();
        let mut limits = Limits::new(stop, Some(LIMIT));
        tokio::time::advance(LIMIT).await;
        stopper.stop(StopReason::Interrupted);
        assert_eq!(
            limits.reached().await,
            Cut::Stopped(StopReason::Interrupted)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_that_comes_as_the_work_ends_is_still_seen() {
        // The work finishes in the same poll as the stop arrives, so
        // `guard` lets its result through: whoever decides the outcome
        // must still ask.
        let (stopper, stop) = stop_signal();
        let mut limits = Limits::new(stop, None);
        let ended = limits
            .guard(async {
                stopper.stop(StopReason::Cancelled);
                "the connection was reset"
            })
            .await;
        assert_eq!(ended, Ok("the connection was reset"));
        assert_eq!(limits.stopped(), Some(StopReason::Cancelled));
    }
}
