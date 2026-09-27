//! How far the runtime's own transfers have got: `progress` events during
//! the `preparing`, `downloading` and `uploading` phases (spec §8.3), so
//! that fetching a large model doesn't look like a hang.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moil_proto::JobEvent;
use tokio::time::{Instant, MissedTickBehavior};

use crate::{EventSink, units};

/// How often a transfer reports, at most. One that takes less time doesn't
/// report at all.
const INTERVAL: Duration = Duration::from_secs(1);

/// Stands for a size not known yet.
const UNKNOWN: u64 = u64::MAX;

/// Counts the bytes a transfer has moved, and how many it will move once
/// that's known. Clones share the counts.
#[derive(Debug, Clone)]
pub(crate) struct Meter(Arc<Counts>);

#[derive(Debug)]
struct Counts {
    moved: AtomicU64,
    total: AtomicU64,
}

impl Default for Meter {
    fn default() -> Self {
        Self(Arc::new(Counts {
            moved: AtomicU64::new(0),
            total: AtomicU64::new(UNKNOWN),
        }))
    }
}

impl Meter {
    pub fn add(&self, bytes: u64) {
        self.0.moved.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Records how many bytes the transfer moves in all.
    pub fn set_total(&self, bytes: u64) {
        self.0.total.store(bytes, Ordering::Relaxed);
    }

    fn moved(&self) -> u64 {
        self.0.moved.load(Ordering::Relaxed)
    }

    fn total(&self) -> Option<u64> {
        Some(self.0.total.load(Ordering::Relaxed)).filter(|&total| total != UNKNOWN)
    }
}

/// Runs `transfer`, which counts what it moves on `meter`. If `events` is
/// given, sends it a `progress` event about `what` (such as "input
/// audio.ogg") every second in which bytes moved, while some are yet to,
/// and a last one when the transfer ends if it sent any.
pub(crate) async fn report<T>(
    events: Option<&EventSink>,
    what: &str,
    meter: &Meter,
    transfer: impl Future<Output = T>,
) -> T {
    let Some(events) = events else {
        return transfer.await;
    };
    let mut ticks = tokio::time::interval_at(Instant::now() + INTERVAL, INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reported = None;
    let send = |reported: &mut Option<u64>, moved: u64| {
        let _ = events.send(event(what, moved, meter.total()));
        *reported = Some(moved);
    };
    tokio::pin!(transfer);
    loop {
        tokio::select! {
            output = &mut transfer => {
                let moved = meter.moved();
                if reported.is_some_and(|reported| reported != moved) {
                    send(&mut reported, moved);
                }
                return output;
            }
            _ = ticks.tick() => {
                let moved = meter.moved();
                let in_flight = moved > 0 && meter.total() != Some(moved);
                if in_flight && reported != Some(moved) {
                    send(&mut reported, moved);
                }
            }
        }
    }
}

/// "input audio.ogg: 12 MiB of 100 MiB", with the fraction when the total
/// is known.
fn event(what: &str, moved: u64, total: Option<u64>) -> JobEvent {
    let (fraction, message) = match total {
        Some(total) if total > 0 => (
            Some((moved as f64 / total as f64).min(1.0)),
            format!("{what}: {} of {}", units::bytes(moved), units::bytes(total)),
        ),
        _ => (None, format!("{what}: {}", units::bytes(moved))),
    };
    JobEvent::Progress {
        fraction,
        message: Some(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_of_unknown_size_reports_what_arrived() {
        assert_eq!(
            event("input audio.ogg", 3 << 20, None),
            JobEvent::Progress {
                fraction: None,
                message: Some("input audio.ogg: 3 MiB".into()),
            }
        );
    }
}
