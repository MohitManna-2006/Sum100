//! Injected engine clock. `WallClock` is the only place in the crate that reads
//! wall-clock time; everything else receives a [`Clock`]. Under replay,
//! [`ReplayClock`] follows recorded local receipt timestamps, never venue `ts`
//! fields, so freshness decisions reproduce exactly.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WallClock;

impl Clock for WallClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Shared handle: the replay feed advances it, the book store reads it.
#[derive(Debug, Clone, Default)]
pub struct ReplayClock(Arc<AtomicU64>);

impl ReplayClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Move to a recorded receipt time. Never moves backwards: receipt times
    /// are wall-clock samples and do not define order (recorder sequence does).
    pub fn advance_to(&self, received_at_ms: u64) {
        self.0.fetch_max(received_at_ms, Ordering::Relaxed);
    }
}

impl Clock for ReplayClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_clock_is_shared_and_monotonic() {
        let clock = ReplayClock::new();
        let reader: Arc<dyn Clock> = Arc::new(clock.clone());
        assert_eq!(reader.now_ms(), 0);
        clock.advance_to(1_789_343_120_404);
        assert_eq!(reader.now_ms(), 1_789_343_120_404);
        clock.advance_to(1_789_343_120_000);
        assert_eq!(reader.now_ms(), 1_789_343_120_404);
    }

    #[test]
    fn wall_clock_reads_epoch_milliseconds() {
        // 2026-01-01T00:00:00Z; a wall clock before this is misconfigured.
        assert!(WallClock.now_ms() > 1_767_225_600_000);
    }
}
