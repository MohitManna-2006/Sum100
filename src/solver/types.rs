//! Solver-internal types: what a violation candidate is, what a priced
//! opportunity is, why candidates get thrown away, and what the engine counts.
//!
//! Every monetary field is integer [`Cents`]. The only float in the module is
//! [`Opportunity::annualized_return`], which ranks candidates that have already
//! passed an integer go/no-go test. A float can never turn a rejected trade into
//! an accepted one here, only reorder accepted ones.

use crate::registry::GroupId;
use crate::types::{Cents, ContractId, Level, Side, Venue};

/// Minimum horizon used when annualizing, in days.
///
/// A contract resolving in the next few minutes would otherwise divide a real
/// edge by something near zero and dominate the ranking with a number that
/// cannot be realized: capital does not actually recycle three hundred times a
/// day. One hour is the floor, so a near-resolution trade ranks high but stays
/// on the same axis as everything else.
pub const MIN_HORIZON_DAYS: f64 = 1.0 / 24.0;

/// The engine's `[engine]` configuration block, in memory.
///
/// Mirrors the `[engine]` block of `config/example.toml`, which
/// [`crate::config::Config`] parses into this. Every field carries a default, so
/// a config that omits the block, or any key in it, still starts.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SolverConfig {
    /// Quotes older than this are stale and never produce a signal.
    pub max_book_age_ms: u64,
    /// Minimum net profit, in integer cents, for a candidate to be reported.
    pub min_net_edge_cents: Cents,
    /// Minimum annualized return on locked capital, as a ratio (0.15 = 15%).
    pub min_annualized_return: f64,
    /// Cap on contracts per leg.
    pub max_position_size: i64,
}

impl Default for SolverConfig {
    fn default() -> Self {
        SolverConfig {
            max_book_age_ms: 500,
            min_net_edge_cents: 1,
            min_annualized_return: 0.15,
            max_position_size: 500,
        }
    }
}

/// One leg of a trade before it has been sized or priced.
///
/// `side` is the outcome being **bought**, not the wire side a resting order
/// sits on. Buying [`Side::No`] consumes resting yes bids, because a yes buy at
/// P and a no buy at `100 - P` are the same match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateLeg {
    pub venue: Venue,
    pub contract_id: ContractId,
    pub side: Side,
}

/// A constraint violation visible at top of book, before costing.
///
/// A candidate is not a signal. It says only that the best prices in the group
/// are mutually inconsistent, which is the cheap test. Depth, fees, freshness,
/// and the return threshold all still get to reject it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub group: GroupId,
    pub legs: Vec<CandidateLeg>,
    /// Sum of best ask prices across the legs, for the rejection log. The gap
    /// against 100 is what fees have to be paid out of.
    pub top_of_book_cost_cents: Cents,
}

/// Result of consuming resting liquidity on one side of one book.
///
/// `consumed` keeps the per-level breakdown rather than only a blended total,
/// because fees round up per level and because a rejected candidate is only
/// explainable if the level structure that killed it is still visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Contracts actually obtainable, which may be less than requested.
    pub filled: i64,
    /// Total premium paid, as size times price summed across levels.
    pub total_cost_cents: Cents,
    /// Levels consumed, cheapest first; the last may be partial.
    pub consumed: Vec<Level>,
}

/// One venue-side fill that forms part of a combined position.
///
/// Legs are retained individually because execution is per venue and per
/// contract: the engine must be able to place, size, and later reconcile each
/// one on its own, and a post-mortem needs to know which leg moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    pub venue: Venue,
    pub contract_id: ContractId,
    /// The outcome bought on this leg.
    pub side: Side,
    /// Contracts bought on this leg.
    pub qty: i64,
    /// Premium paid on this leg.
    pub total_cost_cents: Cents,
    /// Taker fee charged on this leg, summed over the levels consumed.
    pub fee_cents: Cents,
}

/// A fully priced, fully sized violation that is profitable after costs.
///
/// Every field is the post-fee, post-depth truth rather than a headline number,
/// so downstream ranking and risk checks never re-derive costs and never
/// disagree with each other about what the trade is worth.
#[derive(Debug, Clone, PartialEq)]
pub struct Opportunity {
    pub group: GroupId,
    /// The individual fills that make up the position.
    pub legs: Vec<Leg>,
    /// Contracts held on every leg; the set only settles flat if legs match.
    pub qty: i64,
    /// Worst-case payoff over every resolution the relation permits. Not an
    /// assumption: computed by enumerating states in [`crate::registry`].
    pub guaranteed_payoff_cents: Cents,
    /// Cash locked until resolution, which is what the return is measured on.
    pub capital_cents: Cents,
    /// Guaranteed payoff minus premium, before fees.
    pub gross_cents: Cents,
    /// Total taker fees across all legs.
    pub fees_cents: Cents,
    /// Profit actually realized at resolution.
    pub net_cents: Cents,
    /// Days until the locked capital comes back, floored at [`MIN_HORIZON_DAYS`].
    pub days_to_resolution: f64,
    /// Net over capital, scaled to a year. Ranking and display only.
    pub annualized_return: f64,
}

impl Opportunity {
    pub fn annualized_return_percent(&self) -> f64 {
        self.annualized_return * 100.0
    }
}

/// Why a group or candidate produced no signal.
///
/// The breakdown is the deliverable, not a debugging aid. The expected result on
/// live data is that fees reject the majority of candidates, and that claim is
/// only worth anything if every rejection is attributed to one concrete cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// A member book is not `Live`: gap, disconnect, or no snapshot yet.
    NotLive,
    /// Every book is live but at least one is older than `max_book_age_ms`.
    Stale,
    /// A member contract has no book in the store at all.
    MissingBook,
    /// A cross-venue pair a human has not confirmed settles identically.
    Unverified,
    /// At least one leg has no resting liquidity to take.
    NoDepth,
    /// Gross edge was positive but fees consumed all of it. The common case.
    FeesExceedGap,
    /// Profitable, but by less than `min_net_edge_cents`.
    BelowMinEdge,
    /// Profitable, but the return on locked capital is below the threshold.
    BelowMinReturn,
    /// The legs do not actually pay off in every resolution. A relation or fast
    /// path bug; the position is discarded and counted loudly.
    PayoffNotGuaranteed,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::NotLive => "not_live",
            RejectReason::Stale => "stale",
            RejectReason::MissingBook => "missing_book",
            RejectReason::Unverified => "unverified",
            RejectReason::NoDepth => "no_depth",
            RejectReason::FeesExceedGap => "fees_exceed_gap",
            RejectReason::BelowMinEdge => "below_min_edge",
            RejectReason::BelowMinReturn => "below_min_return",
            RejectReason::PayoffNotGuaranteed => "payoff_not_guaranteed",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SolverMetrics {
    pub groups_evaluated: u64,
    pub candidates_found: u64,
    pub opportunities_emitted: u64,
    pub rejected_not_live: u64,
    pub rejected_stale: u64,
    pub rejected_missing_book: u64,
    pub rejected_unverified: u64,
    pub rejected_no_depth: u64,
    pub rejected_fees_exceed_gap: u64,
    pub rejected_below_min_edge: u64,
    pub rejected_below_min_return: u64,
    pub rejected_payoff_not_guaranteed: u64,
}

impl SolverMetrics {
    pub fn note_rejection(&mut self, reason: RejectReason) {
        let slot = match reason {
            RejectReason::NotLive => &mut self.rejected_not_live,
            RejectReason::Stale => &mut self.rejected_stale,
            RejectReason::MissingBook => &mut self.rejected_missing_book,
            RejectReason::Unverified => &mut self.rejected_unverified,
            RejectReason::NoDepth => &mut self.rejected_no_depth,
            RejectReason::FeesExceedGap => &mut self.rejected_fees_exceed_gap,
            RejectReason::BelowMinEdge => &mut self.rejected_below_min_edge,
            RejectReason::BelowMinReturn => &mut self.rejected_below_min_return,
            RejectReason::PayoffNotGuaranteed => &mut self.rejected_payoff_not_guaranteed,
        };
        *slot = slot.saturating_add(1);
    }

    /// Total rejections, for the "fees reject most candidates" breakdown.
    pub fn rejections(&self) -> u64 {
        self.rejected_not_live
            + self.rejected_stale
            + self.rejected_missing_book
            + self.rejected_unverified
            + self.rejected_no_depth
            + self.rejected_fees_exceed_gap
            + self.rejected_below_min_edge
            + self.rejected_below_min_return
            + self.rejected_payoff_not_guaranteed
    }

    pub fn log(&self) {
        tracing::info!(?self, "solver metrics");
    }
}
