pub mod kalshi;
pub mod rest;

use crate::types::{Cents, ContractId, Level, Side, Venue};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEvent {
    /// Both arrays contain resting bids in the named outcome. No complement
    /// conversion or book state lives in the feed. Snapshot time is receipt time
    /// when the venue supplies no timestamp.
    Snapshot {
        contract: ContractId,
        yes: Vec<Level>,
        no: Vec<Level>,
        seq: u64,
        ts_ms: u64,
    },
    Delta {
        contract: ContractId,
        side: Side,
        price: Cents,
        size_delta: i64,
        seq: u64,
        ts_ms: u64,
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
