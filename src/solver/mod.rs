//! Coherence solver: find constraint violations that are still profitable after
//! depth, fees, and staleness have had their say.
//!
//! A set of mutually exclusive and exhaustive prediction market contracts must
//! settle with exactly one leg paying $1. If every leg can be bought for a
//! combined total below 100 cents, the set is incoherent and the difference is
//! locked in regardless of which outcome occurs. The same reasoning covers a
//! contract against its own negation, a threshold ladder against its own
//! ordering, and one event listed on two venues. This module turns those four
//! observations into executable, fee-aware, depth-aware signals.
//!
//! Data flows one way. The book store feeds the solver, the solver feeds the
//! executor, and nothing calls backwards. That is what makes replay reproduce
//! live behavior exactly and keeps concurrency a non-issue: the solver reads
//! books and never mutates anything but its own counters.
//!
//! # Structure
//!
//! - [`fast`] holds the four closed-form checks, which run on every dirty group.
//! - [`costing`] prices, sizes, and gates the handful of candidates they find.
//! - [`types`] defines candidates, opportunities, rejection reasons, and config.
//!
//! # Not implemented: the general fallback
//!
//! Overlapping partial partitions, conditional markets, and multi-leg
//! combinations do not reduce to any of the four shapes. The general method
//! treats the world as a finite set of mutually exclusive states, makes each
//! contract a payoff vector over those states, and asks whether a probability
//! distribution exists under which every price equals its expected payoff. That
//! is a linear feasibility problem whose dual, when infeasible, *is* the trade.
//! It is deliberately future work: it is far more expensive than four additions
//! and a comparison, and the fast paths cover the overwhelming majority of real
//! groups. The two paths must agree where both apply, which is why the state
//! enumeration the fallback would use ([`crate::registry::Relation::resolution_states`])
//! already exists and already gates every signal this module emits.

pub mod costing;
pub mod fast;
pub mod types;

pub use types::{
    Candidate, CandidateLeg, Leg, Opportunity, RejectReason, SolverConfig, SolverMetrics, Walk,
};

use crate::fees::FeeModels;
use crate::registry::{ConstraintGroup, Registry, Relation};
use crate::types::{Book, ContractId};

/// Read-only access to the books the solver evaluates.
///
/// The solver knows about books, constraint groups, and venues, and nothing
/// else — not the executor, not the API, not the feed. This trait is that line.
/// The engine's real book store implements it; so does anything a test wants to
/// hand over, which is also how a mock second venue is exercised before a real
/// Polymarket feed exists.
pub trait BookSource {
    fn book(&self, contract: ContractId) -> Option<&Book>;
    /// Engine time, taken from the same injected clock that stamps books, so
    /// the freshness gate decides identically live and under replay.
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Default)]
pub struct Solver {
    pub metrics: SolverMetrics,
}

impl Solver {
    pub fn new() -> Self {
        Solver::default()
    }

    /// Evaluate every constraint group touched by the dirty contracts.
    ///
    /// Groups are deduplicated and evaluated in group id order, and the returned
    /// opportunities are ranked by annualized return, so the same feed produces
    /// the same output in the same order on every run.
    pub fn evaluate<B: BookSource + ?Sized>(
        &mut self,
        registry: &Registry,
        books: &B,
        dirty: &[ContractId],
        fees: &FeeModels,
        config: &SolverConfig,
    ) -> Vec<Opportunity> {
        let mut groups: Vec<_> = dirty
            .iter()
            .flat_map(|contract| registry.groups_for(*contract))
            .copied()
            .collect();
        groups.sort_unstable();
        groups.dedup();

        let mut opportunities = Vec::new();
        for id in groups {
            let Some(group) = registry.group(id) else {
                continue;
            };
            opportunities.extend(self.evaluate_group(group, books, fees, config));
        }
        costing::rank(&mut opportunities);
        opportunities
    }

    /// Evaluate one group: gate it, run its fast path, then cost what it finds.
    pub fn evaluate_group<B: BookSource + ?Sized>(
        &mut self,
        group: &ConstraintGroup,
        books: &B,
        fees: &FeeModels,
        config: &SolverConfig,
    ) -> Vec<Opportunity> {
        self.metrics.groups_evaluated = self.metrics.groups_evaluated.saturating_add(1);

        let candidates = match self.find_candidates(group, books, config) {
            Ok(candidates) => candidates,
            Err(reason) => {
                self.metrics.note_rejection(reason);
                tracing::debug!(group = group.id.0, %reason, "group not evaluable");
                return Vec::new();
            }
        };

        let now_ms = books.now_ms();
        let mut opportunities = Vec::new();
        for candidate in candidates {
            self.metrics.candidates_found = self.metrics.candidates_found.saturating_add(1);
            match costing::cost_candidate(group, &candidate, books, fees, config, now_ms) {
                Ok(opportunity) => {
                    self.metrics.opportunities_emitted =
                        self.metrics.opportunities_emitted.saturating_add(1);
                    tracing::info!(
                        group = group.id.0,
                        legs = opportunity.legs.len(),
                        qty = opportunity.qty,
                        top_of_book_cost_cents = candidate.top_of_book_cost_cents,
                        total_cost_cents = opportunity.capital_cents - opportunity.fees_cents,
                        fees_cents = opportunity.fees_cents,
                        net_cents = opportunity.net_cents,
                        annualized_return_percent = opportunity.annualized_return_percent(),
                        "opportunity accepted"
                    );
                    opportunities.push(opportunity);
                }
                Err(reason) => {
                    self.metrics.note_rejection(reason);
                    tracing::info!(
                        group = group.id.0,
                        %reason,
                        legs = candidate.legs.len(),
                        top_of_book_cost_cents = candidate.top_of_book_cost_cents,
                        gross_edge_cents = 100 - candidate.top_of_book_cost_cents,
                        "candidate rejected"
                    );
                }
            }
        }
        opportunities
    }

    /// Gather the group's books, apply the freshness gate, and run its check.
    fn find_candidates<B: BookSource + ?Sized>(
        &self,
        group: &ConstraintGroup,
        books: &B,
        config: &SolverConfig,
    ) -> Result<Vec<Candidate>, RejectReason> {
        // A pair no human has confirmed settles identically is rejected before
        // its books are even read: there is nothing to check.
        if let Relation::Equivalent {
            verified: false, ..
        } = group.relation
        {
            return Err(RejectReason::Unverified);
        }

        let mut member_books = Vec::with_capacity(group.members().len());
        for contract in group.members() {
            member_books.push(books.book(*contract).ok_or(RejectReason::MissingBook)?);
        }
        costing::freshness_gate(&member_books, books.now_ms(), config.max_book_age_ms)?;

        // Slice patterns rather than indexing: the registry already guarantees
        // these member counts, and a shape that somehow slipped through should
        // produce no signal rather than panic inside the engine task.
        let mut candidates = Vec::new();
        match &group.relation {
            Relation::Complement { .. } => {
                if let [book] = member_books[..] {
                    fast::evaluate_complement(group.id, book, &mut candidates);
                }
            }
            Relation::Exhaustive { .. } => {
                fast::evaluate_exhaustive(group.id, &member_books, &mut candidates);
            }
            Relation::Monotone { .. } => {
                fast::evaluate_monotonicity(group.id, &member_books, &mut candidates);
            }
            // An implication is a two-rung ladder written the other way round:
            // the consequent is the weaker claim, so it leads the ladder.
            Relation::Implies { .. } => {
                if let [antecedent, consequent] = member_books[..] {
                    fast::evaluate_monotonicity(
                        group.id,
                        &[consequent, antecedent],
                        &mut candidates,
                    );
                }
            }
            Relation::Equivalent { .. } => {
                if let [a, b] = member_books[..] {
                    fast::evaluate_cross_venue(group.id, a, b, &mut candidates);
                }
            }
        }
        Ok(candidates)
    }
}
