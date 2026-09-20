//! The engine task: apply a book update, mark the groups it dirtied, solve
//! them, hand the signals on, publish state.
//!
//! This is the only place the components meet. Feeds produce events, the book
//! store owns state, the registry says what relates to what, the solver decides
//! what is worth trading; none of them know about each other. The engine calls
//! them in a fixed order on every event, which is why the whole path is
//! single-owner and needs no locks: the work per update is microseconds, and
//! splitting it across tasks would add channel hops to parallelize nothing.
//!
//! Determinism is the property to protect. Given the same registry and the same
//! recorded stream, [`Engine::step`] must produce the same signals in the same
//! order as the live run did. Everything time-dependent goes through the
//! injected clock, the dirty set is an ordered `Vec`, and the solver sorts its
//! groups, so nothing here depends on hash iteration order or wall time.

use crate::{
    book::{Applied, BookStore},
    clock::Clock,
    feed::{Feed, FeedEvent},
    fees::FeeModels,
    registry::{GroupId, Registry},
    solver::{Opportunity, Solver, SolverConfig, SolverMetrics},
    types::{BookState, Cents, ContractId, Venue},
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Where accepted signals go.
///
/// Phase 6 replaces the implementation with the paper executor; the engine only
/// needs somewhere to put an opportunity that does not involve knowing what
/// happens to it next.
pub trait OpportunitySink: Send {
    fn accept(&mut self, opportunity: &Opportunity, now_ms: u64);
}

/// One accepted signal, flattened for logging and for replay comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signal {
    pub detected_at_ms: u64,
    pub group: u32,
    pub qty: i64,
    pub capital_cents: Cents,
    pub fees_cents: Cents,
    pub net_cents: Cents,
    pub annualized_return_percent: f64,
    pub days_to_resolution: f64,
    pub legs: Vec<SignalLeg>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SignalLeg {
    pub venue: Venue,
    pub contract: u32,
    /// The outcome bought. Every leg of an arbitrage is a purchase; selling an
    /// outcome on these venues is buying its complement.
    pub side: &'static str,
    pub qty: i64,
    pub cost_cents: Cents,
    pub fee_cents: Cents,
}

/// Records every accepted signal in order. The phase 5 stand-in for the
/// executor, and the thing replay determinism is asserted over.
#[derive(Debug, Default)]
pub struct SignalLog {
    pub signals: Vec<Signal>,
}

impl OpportunitySink for SignalLog {
    fn accept(&mut self, opportunity: &Opportunity, now_ms: u64) {
        self.signals.push(Signal {
            detected_at_ms: now_ms,
            group: opportunity.group.0,
            qty: opportunity.qty,
            capital_cents: opportunity.capital_cents,
            fees_cents: opportunity.fees_cents,
            net_cents: opportunity.net_cents,
            annualized_return_percent: opportunity.annualized_return_percent(),
            days_to_resolution: opportunity.days_to_resolution,
            legs: opportunity
                .legs
                .iter()
                .map(|leg| SignalLeg {
                    venue: leg.venue,
                    contract: leg.contract_id.0,
                    side: match leg.side {
                        crate::types::Side::Yes => "yes",
                        crate::types::Side::No => "no",
                    },
                    qty: leg.qty,
                    cost_cents: leg.total_cost_cents,
                    fee_cents: leg.fee_cents,
                })
                .collect(),
        });
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct EngineMetrics {
    pub events_received: u64,
    pub books_updated: u64,
    pub contracts_marked_dirty: u64,
    pub groups_dirtied: u64,
    pub signals_accepted: u64,
    pub sequence_gaps: u64,
    pub resyncs_requested: u64,
    pub states_broadcast: u64,
}

/// A contract's book, compressed to what a dashboard actually shows.
///
/// Full ladders are deliberately not published. A broadcast happens on every
/// applied event, and copying a hundred levels per contract per delta would
/// cost more than the solve it follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractSummary {
    pub contract: u32,
    pub venue: Venue,
    pub ticker: String,
    pub state: &'static str,
    pub seq: u64,
    pub best_bid: Option<Cents>,
    pub best_ask: Option<Cents>,
    pub age_ms: u64,
}

/// What the API layer serializes. Published after every applied update.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineState {
    pub timestamp_ms: u64,
    pub contracts: Vec<ContractSummary>,
    pub opportunities: Vec<Signal>,
    pub engine: EngineMetrics,
    pub solver: SolverMetrics,
}

/// Generic over the sink rather than boxed: the caller keeps the concrete type,
/// so a replay can read its own signal log back without a lock in the hot path.
pub struct Engine<S: OpportunitySink> {
    registry: Registry,
    book_store: BookStore,
    solver: Solver,
    sink: S,
    fees: FeeModels,
    config: SolverConfig,
    clock: Arc<dyn Clock>,
    /// Set by a sequence gap, consumed by the run loop, which owns the feed.
    resync_pending: bool,
    pub metrics: EngineMetrics,
}

impl<S: OpportunitySink> Engine<S> {
    pub fn new(
        registry: Registry,
        book_store: BookStore,
        solver: Solver,
        sink: S,
        fees: FeeModels,
        config: SolverConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Engine {
            registry,
            book_store,
            solver,
            sink,
            fees,
            config,
            clock,
            resync_pending: false,
            metrics: EngineMetrics::default(),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn book_store(&self) -> &BookStore {
        &self.book_store
    }

    pub fn solver_metrics(&self) -> &SolverMetrics {
        &self.solver.metrics
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Apply one event and solve whatever it dirtied.
    ///
    /// Synchronous and self-contained on purpose: the whole decision path is
    /// testable without a runtime, a socket, or a clock that moves.
    pub fn step(&mut self, event: &FeedEvent) -> Vec<Opportunity> {
        self.metrics.events_received = self.metrics.events_received.saturating_add(1);
        let (applied, dirty) = self.book_store.apply_and_mark(event);

        if let Applied::Gap { expected, got } = applied {
            self.metrics.sequence_gaps = self.metrics.sequence_gaps.saturating_add(1);
            // Do not repair the books. Ask for a fresh snapshot and evaluate
            // nothing until one lands: a book that is subtly wrong is the one
            // failure mode this system cannot tolerate.
            self.resync_pending = true;
            tracing::warn!(expected, got, "sequence gap; books held until resync");
        }
        if dirty.is_empty() {
            return Vec::new();
        }
        self.metrics.books_updated = self.metrics.books_updated.saturating_add(1);
        self.metrics.contracts_marked_dirty = self
            .metrics
            .contracts_marked_dirty
            .saturating_add(dirty.len() as u64);
        self.metrics.groups_dirtied = self
            .metrics
            .groups_dirtied
            .saturating_add(self.dirty_group_count(&dirty));

        // The solver deduplicates and orders the groups itself, so a contract in
        // several groups, or several dirty contracts sharing one, still costs
        // exactly one evaluation per group per event.
        let opportunities = self.solver.evaluate(
            &self.registry,
            &self.book_store,
            &dirty,
            &self.fees,
            &self.config,
        );
        let now_ms = self.clock.now_ms();
        for opportunity in &opportunities {
            self.metrics.signals_accepted = self.metrics.signals_accepted.saturating_add(1);
            self.sink.accept(opportunity, now_ms);
        }
        opportunities
    }

    fn dirty_group_count(&self, dirty: &[ContractId]) -> u64 {
        let mut groups: Vec<GroupId> = dirty
            .iter()
            .flat_map(|contract| self.registry.groups_for(*contract))
            .copied()
            .collect();
        groups.sort_unstable();
        groups.dedup();
        groups.len() as u64
    }

    /// Drive a feed to exhaustion, publishing state as it goes.
    ///
    /// Returns when the feed closes: end of file under replay, or a stopped
    /// socket live. A sequence gap asks the feed to resync rather than trying to
    /// repair the books, because a repaired book that is subtly wrong is the one
    /// failure this system cannot tolerate.
    pub async fn run<F: Feed + ?Sized>(
        &mut self,
        feed: &mut F,
        broadcast: &broadcast::Sender<EngineState>,
    ) {
        while let Some(event) = feed.next().await {
            let opportunities = self.step(&event);
            if std::mem::take(&mut self.resync_pending) {
                feed.request_resync();
                self.book_store.note_resync_request();
                self.metrics.resyncs_requested = self.metrics.resyncs_requested.saturating_add(1);
            }
            self.publish(broadcast, &opportunities);
        }
    }

    /// Publish engine state, but only build it when somebody is listening.
    ///
    /// A verification replay has no subscribers, and there is no reason for it
    /// to allocate a snapshot per event that nothing will read.
    fn publish(&mut self, broadcast: &broadcast::Sender<EngineState>, latest: &[Opportunity]) {
        if broadcast.receiver_count() == 0 {
            return;
        }
        let state = self.state(latest);
        if broadcast.send(state).is_ok() {
            self.metrics.states_broadcast = self.metrics.states_broadcast.saturating_add(1);
        }
    }

    pub fn state(&self, latest: &[Opportunity]) -> EngineState {
        let now_ms = self.clock.now_ms();
        let mut log = SignalLog::default();
        for opportunity in latest {
            log.accept(opportunity, now_ms);
        }
        EngineState {
            timestamp_ms: now_ms,
            contracts: self
                .book_store
                .books()
                .iter()
                .map(|book| ContractSummary {
                    contract: book.contract_id.0,
                    venue: book.venue,
                    ticker: self
                        .book_store
                        .contracts()
                        .resolve(book.contract_id)
                        .map(|(_, ticker)| ticker.clone())
                        .unwrap_or_default(),
                    state: match book.state {
                        BookState::Uninitialized => "uninitialized",
                        BookState::Resyncing => "resyncing",
                        BookState::Live => "live",
                    },
                    seq: book.seq,
                    best_bid: book.best_bid().map(|level| level.price),
                    best_ask: book.best_ask().map(|level| level.price),
                    age_ms: now_ms.saturating_sub(book.updated_at_ms),
                })
                .collect(),
            opportunities: log.signals,
            engine: self.metrics.clone(),
            solver: self.solver.metrics.clone(),
        }
    }
}
