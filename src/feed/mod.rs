pub mod kalshi;
pub mod replay;
pub mod rest;

use crate::metrics::Metrics;
use crate::types::{Cents, ContractId, Level, Side, Venue};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEvent {
    /// Both arrays contain resting bids in the named outcome. No complement
    /// conversion or book state lives in the feed.
    Snapshot {
        contract: ContractId,
        yes: Vec<Level>,
        no: Vec<Level>,
        seq: u64,
        /// The venue's own timestamp, if the payload has one (Kalshi snapshots
        /// currently do not). Never local receipt time, which comes from the
        /// injected clock. Kept for clock-skew tracking; nothing reads it yet.
        venue_ts_ms: Option<u64>,
    },
    Delta {
        contract: ContractId,
        side: Side,
        price: Cents,
        size_delta: i64,
        seq: u64,
        /// The venue's own timestamp; required on Kalshi deltas.
        venue_ts_ms: u64,
    },
    Disconnected {
        venue: Venue,
    },
    Resubscribed {
        contract: ContractId,
    },
}

/// Object-safe async boundary without adding an async-trait dependency.
pub trait Feed: Send {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>>;

    /// Ask the venue for a fresh snapshot after a sequence gap.
    ///
    /// The default does nothing, which is right for a recorded stream: a replay
    /// already contains whatever resync the live run performed, and inventing a
    /// second one would desynchronize it from the recording. A live feed
    /// overrides this to drop its socket and let the reconnect path deliver the
    /// snapshot. Having it on the trait means the engine loop handles a gap the
    /// same way for every feed instead of knowing which one it holds.
    fn request_resync(&self) {}

    /// The feed's own counters as they stand now, for the dashboard.
    ///
    /// Parse errors and reconnections are only ever visible here: a payload the
    /// parser rejects produces no event, so nothing downstream can count what it
    /// never saw. The default is `None` rather than a zeroed [`Metrics`],
    /// because a feed that does not keep these has no answer, and zeros would
    /// reach the dashboard as a flawless venue rather than an unmeasured one.
    fn metrics(&self) -> Option<Metrics> {
        None
    }
}
