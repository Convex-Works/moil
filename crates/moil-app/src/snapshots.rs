//! The agent's snapshots, and the home's reports, taken by the event loop
//! when it gets to them.
//!
//! A script reporting progress in a tight loop changes the snapshot far
//! more often than the tray can be redrawn. Waking the event loop once per
//! change filled its queue faster than it emptied; instead it is woken once,
//! and takes the latest snapshot when it gets to it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moil_agent::Snapshot;
use tokio::runtime::Handle;
use tokio::sync::watch;

/// The latest snapshot, or other watched value, for the event loop.
#[derive(Debug)]
pub struct Snapshots<T = Snapshot> {
    latest: watch::Receiver<T>,
    /// Whether the event loop was woken and hasn't taken the snapshot yet.
    woken: Arc<AtomicBool>,
}

impl<T: Clone + Send + Sync + 'static> Snapshots<T> {
    /// Watches `snapshots` on `runtime`, calling `wake` when there is a new
    /// one to [`take`](Self::take), and not again until it has been taken.
    /// `wake` returns false once there's no one left to wake.
    pub fn watch(
        snapshots: watch::Receiver<T>,
        runtime: &Handle,
        wake: impl Fn() -> bool + Send + 'static,
    ) -> Self {
        let woken = Arc::new(AtomicBool::new(false));
        let mut changes = snapshots.clone();
        let pending = woken.clone();
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                if !pending.swap(true, Ordering::SeqCst) && !wake() {
                    return;
                }
            }
        });
        Self {
            latest: snapshots,
            woken,
        }
    }

    /// The latest snapshot. A change after this wakes the event loop again.
    pub fn take(&mut self) -> T {
        self.woken.store(false, Ordering::SeqCst);
        self.latest.borrow_and_update().clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[tokio::test]
    async fn a_burst_of_snapshots_wakes_the_event_loop_once() {
        let (agent, snapshots) = watch::channel(Snapshot::default());
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = wakes.clone();
        let mut snapshots: Snapshots = Snapshots::watch(snapshots, &Handle::current(), move || {
            counted.fetch_add(1, Ordering::SeqCst);
            true
        });

        // The watcher sees every change, but the event loop is busy.
        for i in 1..=100 {
            agent.send_modify(|s| s.paused = i % 2 == 1);
            tokio::task::yield_now().await;
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert!(!snapshots.take().paused, "the latest one");

        agent.send_modify(|s| s.paused = true);
        tokio::task::yield_now().await;
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            2,
            "once taken, it wakes again"
        );
        assert!(snapshots.take().paused);
    }
}
