pub mod kalshi;
pub mod merged;
pub mod polymarket;
pub mod replay;
pub mod rest;

use crate::metrics::Metrics;
use crate::types::{Cents, ContractId, Level, Side, TokenSide, Venue};
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
    /// One price level restated outright, for a venue that publishes the size
    /// now resting at a price rather than the change to it.
    ///
    /// Carries no sequence number because the venues that send this do not
    /// number their streams. A counter minted here on receipt could never
    /// disagree with itself, so it would detect no gap it was not itself
    /// inventing; continuity for such a feed has to come from re-requesting the
    /// book, not from a number this process made up.
    LevelSet {
        contract: ContractId,
        side: TokenSide,
        price: Cents,
        /// The size now resting at this price, not a change to it.
        size: i64,
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
    /// The next event, or `None` when the feed is finished.
    ///
    /// # Must be cancel-safe
    ///
    /// [`merged::MergedFeed`] selects across several feeds and drops the
    /// losing future, so an implementation must not leave state half-advanced
    /// at an await point. Anything read from the transport before an await has
    /// to be buffered where the next call will find it, and any clock or
    /// counter that belongs to it must move at the same time — not afterwards.
    /// An implementation that breaks this loses events silently, and only when
    /// two venues happen to be busy at once.
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>>;

    /// The venue this feed carries, or `None` when it carries several.
    fn venue(&self) -> Option<Venue> {
        None
    }

    /// Ask the venue for a fresh snapshot after a sequence gap.
    ///
    /// The default does nothing, which is right for a recorded stream: a replay
    /// already contains whatever resync the live run performed, and inventing a
    /// second one would desynchronize it from the recording. A live feed
    /// overrides this to drop its socket and let the reconnect path deliver the
    /// snapshot. Having it on the trait means the engine loop handles a gap the
    /// same way for every feed instead of knowing which one it holds.
    ///
    /// The venue is named because a gap is a fact about one subscription. A
    /// feed carrying several must not drop the others' sockets to repair one:
    /// their sequences came from different handshakes and are still intact, and
    /// resyncing them throws away live books to fix a venue that was fine.
    fn request_resync(&self, _venue: Venue) {}

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

    /// Counters attributed to the venue that produced them.
    ///
    /// The dashboard reports health per venue, so a single total would show
    /// both venues one sum and make a dead feed indistinguishable from a busy
    /// one sitting beside it. A single-venue feed answers from [`Feed::venue`]
    /// and [`Feed::metrics`]; a feed carrying several overrides this.
    fn metrics_by_venue(&self) -> Vec<(Venue, Metrics)> {
        match (self.venue(), self.metrics()) {
            (Some(venue), Some(metrics)) => vec![(venue, metrics)],
            _ => Vec::new(),
        }
    }
}
