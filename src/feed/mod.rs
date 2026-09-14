pub mod kalshi;
pub mod replay;
pub mod rest;

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
}
