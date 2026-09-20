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
//!
//! # Deciding and spending are separate
//!
//! [`Engine::step`] is synchronous and touches no network: apply, solve, and
//! decide what *would* be traded. [`Engine::execute`] is the async half that
//! places orders. Keeping them apart means the decision path stays testable
//! without a runtime, and a slow venue cannot delay the book keeping behind it.
//!
//! Between them sits admission, where a profitable trade gets refused for
//! reasons that have nothing to do with its edge: the venue is not healthy,
//! there is no capital, the position would concentrate too much in one event or
//! theme. Every refusal is counted and named.

use crate::{
    book::{Applied, BookStore},
    clock::Clock,
    exec::{
        NewOrder, OrderAction, OrderClient, OrderFill, TimeInForce,
        atomic::{AtomicTrade, DEFAULT_TIMEOUT_MS, ExecutionError},
    },
    feed::{Feed, FeedEvent},
    fees::FeeModels,
    health::{HealthMonitor, HealthState},
    portfolio::{Portfolio, PortfolioSummary, Position, PositionLeg},
    registry::{EventId, GroupId, Registry},
    risk::{self, RiskLimits, RiskStatus},
    solver::{
        Opportunity, Solver, SolverConfig, SolverMetrics,
        costing::{walk_depth_and_cost, worst_price_cents},
    },
    types::{BookState, Cents, ContractId, Venue},
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Everything the engine needs beyond its components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    pub solver: SolverConfig,
    pub risk: RiskLimits,
    pub max_idle_ms: u64,
    pub atomic_timeout_ms: u64,
    /// Whether a live [`OrderClient`] may be used. Defaults to false, and the
    /// engine refuses a live client without it: forgetting a flag must not be
    /// the difference between a simulation and real money.
    pub allow_live_orders: bool,
    /// Whether an auto-discovered group may be traded with a live client.
    ///
    /// Separate from `allow_live_orders` because it is a different question. A
    /// hand-written group says a human read the resolution rules; an inferred
    /// one says the venue's metadata was read correctly. The solver prices every
    /// trade against its relation's own resolution states, so a relation that is
    /// wrong yields a confidently wrong guaranteed payoff — which is the worst
    /// possible input to something that can place orders.
    pub allow_inferred_live_orders: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            solver: SolverConfig::default(),
            risk: RiskLimits::default(),
            max_idle_ms: crate::health::DEFAULT_MAX_IDLE_MS,
            atomic_timeout_ms: DEFAULT_TIMEOUT_MS,
            allow_live_orders: false,
            allow_inferred_live_orders: false,
        }
    }
}

/// Where accepted signals go.
///
/// Phase 6 replaces the implementation with the paper executor; the engine only
/// needs somewhere to put an opportunity that does not involve knowing what
/// happens to it next.
pub trait OpportunitySink: Send {
    fn accept(&mut self, opportunity: &Opportunity, fills: &[OrderFill], now_ms: u64);
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
    /// Venue order ids backing this signal. Empty means the trade was decided
    /// but never placed, which is what a dry run looks like.
    pub order_ids: Vec<String>,
    /// What the fills actually cost, which is not always what was quoted.
    pub filled_cost_cents: Cents,
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
    fn accept(&mut self, opportunity: &Opportunity, fills: &[OrderFill], now_ms: u64) {
        self.signals.push(Signal {
            order_ids: fills.iter().map(|f| f.order_id.clone()).collect(),
            filled_cost_cents: fills.iter().map(OrderFill::cost_cents).sum(),
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
    /// Admission refusals, by cause: a profitable trade the engine declined.
    pub blocked_unhealthy: u64,
    pub blocked_no_capital: u64,
    pub blocked_risk_limit: u64,
    pub blocked_unbound_contract: u64,
    /// Live orders withheld from a group nobody has confirmed by hand.
    pub blocked_inferred_group: u64,
    pub trades_placed: u64,
    pub trades_no_fill: u64,
    /// Legged or timed out: a position may exist that nobody planned.
    pub trades_needing_reconciliation: u64,
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
    pub portfolio: PortfolioSummary,
    pub health: HealthStatus,
    pub risk: RiskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HealthStatus {
    pub kalshi_healthy: bool,
    pub kalshi_state: HealthState,
    pub last_message_age_ms: u64,
}

/// A trade that passed every gate and is ready to place.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTrade {
    pub opportunity: Opportunity,
    pub event: EventId,
    pub trade: AtomicTrade,
}

/// Generic over the sink rather than boxed: the caller keeps the concrete type,
/// so a replay can read its own signal log back without a lock in the hot path.
pub struct Engine<S: OpportunitySink> {
    registry: Registry,
    book_store: BookStore,
    solver: Solver,
    sink: S,
    fees: FeeModels,
    config: EngineConfig,
    clock: Arc<dyn Clock>,
    /// Set by a sequence gap, consumed by the run loop, which owns the feed.
    resync_pending: bool,
    pub portfolio: Portfolio,
    pub health: HealthMonitor,
    pub metrics: EngineMetrics,
}

impl<S: OpportunitySink> Engine<S> {
    pub fn new(
        registry: Registry,
        book_store: BookStore,
        sink: S,
        fees: FeeModels,
        config: EngineConfig,
        clock: Arc<dyn Clock>,
        portfolio: Portfolio,
    ) -> Self {
        Engine {
            registry,
            book_store,
            solver: Solver::new(),
            sink,
            fees,
            config,
            clock,
            resync_pending: false,
            portfolio,
            health: HealthMonitor::new(config.max_idle_ms),
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
        let now_ms = self.clock.now_ms();
        // Health folds in every event, including the ones that dirty nothing:
        // a disconnect is exactly the event that must stop trading.
        self.health.apply_feed_event(event, now_ms);
        self.portfolio.reset_daily_if_needed(now_ms);
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
        self.solver.evaluate(
            &self.registry,
            &self.book_store,
            &dirty,
            &self.fees,
            &self.config.solver,
        )
    }

    /// Decide which profitable trades are allowed to happen, and price the legs.
    ///
    /// This is the gap between "the arithmetic says yes" and "we are willing to
    /// do it". Health first, because a trade decided on a dead feed is priced
    /// off a fiction; then capital, then concentration. Each refusal is counted
    /// so the reason a good signal was skipped is recoverable afterwards.
    pub fn admit(&mut self, opportunities: Vec<Opportunity>) -> Vec<PlannedTrade> {
        if opportunities.is_empty() {
            return Vec::new();
        }
        let now_ms = self.clock.now_ms();
        if !self.health.can_trade(Venue::Kalshi, now_ms) {
            self.metrics.blocked_unhealthy = self
                .metrics
                .blocked_unhealthy
                .saturating_add(opportunities.len() as u64);
            tracing::warn!(
                skipped = opportunities.len(),
                state = ?self.health.kalshi.state,
                "venue unhealthy; signals not traded"
            );
            return Vec::new();
        }

        let mut planned = Vec::new();
        for opportunity in opportunities {
            let Some(event) = self.event_of(&opportunity) else {
                self.metrics.blocked_unbound_contract =
                    self.metrics.blocked_unbound_contract.saturating_add(1);
                continue;
            };
            if !self.portfolio.can_afford(opportunity.capital_cents)
                || !self.portfolio.within_daily_loss_limit()
            {
                self.metrics.blocked_no_capital = self.metrics.blocked_no_capital.saturating_add(1);
                tracing::info!(
                    group = opportunity.group.0,
                    needed = opportunity.capital_cents,
                    available = self.portfolio.capital_available_cents,
                    "signal skipped: capital"
                );
                continue;
            }
            let theme = self.registry.event(event).and_then(|e| e.theme.clone());
            if let Err(error) = risk::check(
                &self.portfolio,
                &self.registry,
                event,
                theme.as_deref(),
                opportunity.capital_cents,
                &self.config.risk,
            ) {
                self.metrics.blocked_risk_limit = self.metrics.blocked_risk_limit.saturating_add(1);
                tracing::info!(group = opportunity.group.0, %error, "signal skipped: risk");
                continue;
            }
            match self.plan_legs(&opportunity) {
                Some(legs) => planned.push(PlannedTrade {
                    opportunity,
                    event,
                    trade: AtomicTrade {
                        legs,
                        timeout_ms: self.config.atomic_timeout_ms,
                    },
                }),
                None => {
                    self.metrics.blocked_unbound_contract =
                        self.metrics.blocked_unbound_contract.saturating_add(1);
                }
            }
        }
        planned
    }

    /// Which canonical event a signal belongs to, via its group's first member.
    ///
    /// Every member of a group is bound to the same event; the registry refuses
    /// to load one that is not, so reading the first is enough.
    fn event_of(&self, opportunity: &Opportunity) -> Option<EventId> {
        let group = self.registry.group(opportunity.group)?;
        let first = group.members().first()?;
        self.registry.binding(*first).map(|binding| binding.event)
    }

    /// Turn priced legs into orders the venue will accept.
    ///
    /// The limit is the worst level the solver's walk actually consumed, not
    /// the blended average: a limit at the average would have the venue refuse
    /// the deeper half of the very fill that was costed.
    fn plan_legs(&self, opportunity: &Opportunity) -> Option<Vec<NewOrder>> {
        let mut orders = Vec::with_capacity(opportunity.legs.len());
        for (index, leg) in opportunity.legs.iter().enumerate() {
            let ticker = self.registry.binding(leg.contract_id)?.venue_ticker.clone();
            let book = self.book_store.get(leg.contract_id)?;
            let walk = walk_depth_and_cost(book, leg.side, leg.qty);
            let limit_price = worst_price_cents(&walk.consumed, leg.qty)?;
            orders.push(NewOrder {
                contract_id: leg.contract_id,
                ticker,
                outcome: leg.side,
                action: OrderAction::Buy,
                quantity: leg.qty,
                limit_price,
                // Every leg of an arbitrage is all-or-nothing; a partial turns
                // a risk-free position into a directional one.
                time_in_force: TimeInForce::FillOrKill,
                client_order_id: format!(
                    "sum100-{}-{}-{index}",
                    opportunity.group.0,
                    self.clock.now_ms()
                ),
            });
        }
        Some(orders)
    }

    /// Place the planned trades and record what came back.
    ///
    /// A live client without the explicit opt-in is refused here rather than at
    /// construction, so the check sits on the path that spends money.
    pub async fn execute(
        &mut self,
        planned: Vec<PlannedTrade>,
        client: &dyn OrderClient,
    ) -> Vec<Signal> {
        if planned.is_empty() {
            return Vec::new();
        }
        if client.is_live() && !self.config.allow_live_orders {
            tracing::error!(
                planned = planned.len(),
                "REFUSING a live order client: live trading was not opted into"
            );
            return Vec::new();
        }
        client.observe_books(self.book_store.books(), self.clock.now_ms());

        let mut signals = Vec::new();
        for plan in planned {
            if client.is_live()
                && !self.config.allow_inferred_live_orders
                && self
                    .registry
                    .group(plan.opportunity.group)
                    .is_some_and(|group| group.inferred)
            {
                self.metrics.blocked_inferred_group =
                    self.metrics.blocked_inferred_group.saturating_add(1);
                tracing::warn!(
                    group = plan.opportunity.group.0,
                    "withholding a live order: this group was inferred, not verified by a human"
                );
                continue;
            }
            match plan.trade.execute(client).await {
                Ok(result) => {
                    let now_ms = self.clock.now_ms();
                    let position = position_from(&plan, &result.fills, now_ms);
                    if let Err(error) = self.portfolio.add_position(position) {
                        // The fills are already real, so this is a book keeping
                        // failure on a live position, not a refusal to trade.
                        self.metrics.trades_needing_reconciliation =
                            self.metrics.trades_needing_reconciliation.saturating_add(1);
                        tracing::error!(%error, "filled a trade the portfolio rejected");
                        continue;
                    }
                    self.metrics.trades_placed = self.metrics.trades_placed.saturating_add(1);
                    self.metrics.signals_accepted = self.metrics.signals_accepted.saturating_add(1);
                    self.sink.accept(&plan.opportunity, &result.fills, now_ms);
                    signals.push(Signal {
                        detected_at_ms: now_ms,
                        group: plan.opportunity.group.0,
                        qty: plan.opportunity.qty,
                        capital_cents: plan.opportunity.capital_cents,
                        fees_cents: plan.opportunity.fees_cents,
                        net_cents: plan.opportunity.net_cents,
                        annualized_return_percent: plan.opportunity.annualized_return_percent(),
                        days_to_resolution: plan.opportunity.days_to_resolution,
                        legs: signal_legs(&plan.opportunity),
                        order_ids: result.fills.iter().map(|f| f.order_id.clone()).collect(),
                        filled_cost_cents: result.total_cost_cents(),
                    });
                }
                Err(ExecutionError::NoFills { failed }) => {
                    self.metrics.trades_no_fill = self.metrics.trades_no_fill.saturating_add(1);
                    self.health.note_error(Venue::Kalshi);
                    tracing::info!(
                        group = plan.opportunity.group.0,
                        failures = failed.len(),
                        "trade missed"
                    );
                }
                Err(error) => {
                    // Legged or timed out: something may be live that nobody
                    // planned, and no further trading should paper over it.
                    self.metrics.trades_needing_reconciliation =
                        self.metrics.trades_needing_reconciliation.saturating_add(1);
                    self.health.note_error(Venue::Kalshi);
                    tracing::error!(group = plan.opportunity.group.0, %error, "trade needs reconciliation");
                }
            }
        }
        signals
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
        client: &dyn OrderClient,
        broadcast: &broadcast::Sender<EngineState>,
    ) {
        while let Some(event) = feed.next().await {
            let opportunities = self.step(&event);
            if std::mem::take(&mut self.resync_pending) {
                feed.request_resync();
                self.book_store.note_resync_request();
                self.metrics.resyncs_requested = self.metrics.resyncs_requested.saturating_add(1);
            }
            let planned = self.admit(opportunities);
            let signals = self.execute(planned, client).await;
            self.mark_to_market();
            self.publish(broadcast, &signals);
        }
    }

    /// Revalue open positions against the books as they stand now.
    fn mark_to_market(&mut self) {
        let store = &self.book_store;
        self.portfolio
            .mark_to_market(|contract| store.get(contract));
    }

    /// Publish engine state, but only build it when somebody is listening.
    ///
    /// A verification replay has no subscribers, and there is no reason for it
    /// to allocate a snapshot per event that nothing will read.
    fn publish(&mut self, broadcast: &broadcast::Sender<EngineState>, latest: &[Signal]) {
        if broadcast.receiver_count() == 0 {
            return;
        }
        let state = self.state(latest);
        if broadcast.send(state).is_ok() {
            self.metrics.states_broadcast = self.metrics.states_broadcast.saturating_add(1);
        }
    }

    pub fn state(&self, latest: &[Signal]) -> EngineState {
        let now_ms = self.clock.now_ms();
        let kalshi = &self.health.kalshi;
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
            opportunities: latest.to_vec(),
            engine: self.metrics.clone(),
            solver: self.solver.metrics.clone(),
            portfolio: self.portfolio.summary(),
            health: HealthStatus {
                kalshi_healthy: kalshi.is_healthy(),
                kalshi_state: kalshi.state,
                last_message_age_ms: kalshi.idle_ms(now_ms),
            },
            risk: self.risk_status(),
        }
    }

    /// Why the engine would refuse to trade right now, if it would.
    ///
    /// Reported even when nothing is blocking, so a dashboard can show the
    /// railings rather than only their failures.
    fn risk_status(&self) -> RiskStatus {
        let mut reasons = Vec::new();
        if !self.health.kalshi.is_healthy() {
            reasons.push(format!("kalshi {:?}", self.health.kalshi.state).to_lowercase());
        }
        if !self.portfolio.within_daily_loss_limit() {
            reasons.push("daily loss limit reached".into());
        }
        if self.portfolio.open_count() >= self.config.risk.max_concurrent_trades {
            reasons.push("concurrent trade limit".into());
        }
        if self.portfolio.capital_available_cents <= 0 {
            reasons.push("no capital available".into());
        }
        RiskStatus {
            can_trade: reasons.is_empty(),
            reasons,
            open_positions: self.portfolio.open_count(),
            limits: self.config.risk,
        }
    }
}

/// Build the position a set of fills created.
///
/// Cost comes from the fills, not from the opportunity that predicted them:
/// what was actually paid is what is actually at risk.
fn position_from(plan: &PlannedTrade, fills: &[OrderFill], now_ms: u64) -> Position {
    Position {
        event_id: plan.event,
        group: plan.opportunity.group.0,
        legs: fills
            .iter()
            .map(|fill| PositionLeg {
                contract_id: fill.contract_id,
                side: fill.outcome,
                quantity: fill.quantity_filled,
                average_entry_price: fill.average_price,
                order_ids: vec![fill.order_id.clone()],
            })
            .collect(),
        cost_cents: fills.iter().map(OrderFill::cost_cents).sum(),
        fees_cents: fills.iter().map(|fill| fill.fee_cents).sum(),
        entry_time_ms: now_ms,
        resolved: false,
        pnl_realized_cents: 0,
        pnl_unrealized_cents: 0,
    }
}

fn signal_legs(opportunity: &Opportunity) -> Vec<SignalLeg> {
    opportunity
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
        .collect()
}
