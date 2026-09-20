//! Costing: turn a top-of-book violation into a priced, sized, fee-aware trade.
//!
//! A candidate is a claim about the best prices in a group. Costing is where
//! that claim meets the rest of the order book, the venue's fee schedule, and
//! the clock. On live data this stage rejects the large majority of candidates,
//! almost always because fees exceed the gap, and that is the correct outcome
//! rather than a sign the detector is too eager.
//!
//! Every quantity here is integer [`Cents`]. The profitability decision is made
//! entirely in integers; the float appears only after a trade has been accepted,
//! to rank it against other accepted trades.

use crate::fees::{FeeModel, FeeModels};
use crate::registry::{ConstraintGroup, Relation};
use crate::solver::BookSource;
use crate::solver::types::{
    Candidate, CandidateLeg, Leg, MIN_HORIZON_DAYS, Opportunity, RejectReason, SolverConfig, Walk,
};
use crate::types::{Book, Cents, Level, Side};

/// A binary contract settles at exactly $1 or $0.
pub const PAYOFF_PER_CONTRACT_CENTS: Cents = 100;

/// Milliseconds in a day, for the horizon arithmetic.
const MS_PER_DAY: f64 = 86_400_000.0;

/// Every participating book must be `Live` and recently updated.
///
/// This runs before any check, not after, because the arithmetic of a
/// constraint only holds if every price is simultaneously true. Acting on one
/// quote that has already moved is how a risk-free position becomes a
/// directional loss, and a book that is not `Live` may have silently diverged
/// from the venue's state, which is worse than having no book at all.
pub fn freshness_gate(books: &[&Book], now_ms: u64, max_age_ms: u64) -> Result<(), RejectReason> {
    if books
        .iter()
        .any(|b| b.state != crate::types::BookState::Live)
    {
        return Err(RejectReason::NotLive);
    }
    if !books.iter().all(|b| b.is_fresh(now_ms, max_age_ms)) {
        return Err(RejectReason::Stale);
    }
    Ok(())
}

/// Walk one side of one book from the cheapest price upward, up to `want`.
///
/// The per-level breakdown is kept rather than a blended average because fees
/// round up per level, and because a rejection is only explainable afterwards if
/// the level structure that caused it is still visible.
pub fn walk_depth_and_cost(book: &Book, side: Side, want: i64) -> Walk {
    match side {
        Side::Yes => walk_levels(book.asks(), want),
        Side::No => walk_levels(book.no_asks(), want),
    }
}

fn walk_levels(levels: impl Iterator<Item = Level>, want: i64) -> Walk {
    let mut walk = Walk {
        filled: 0,
        total_cost_cents: 0,
        consumed: Vec::new(),
    };
    if want <= 0 {
        return walk;
    }
    for level in levels {
        let remaining = want - walk.filled;
        if remaining <= 0 {
            break;
        }
        let take = remaining.min(level.size);
        if take <= 0 {
            continue;
        }
        walk.filled += take;
        walk.total_cost_cents += take * level.price;
        walk.consumed.push(Level {
            price: level.price,
            size: take,
        });
    }
    walk
}

/// Premium paid to take `qty` contracts from an already-walked ladder.
pub fn total_cost_cents(ladder: &[Level], qty: i64) -> Cents {
    let mut remaining = qty.max(0);
    let mut cost = 0;
    for level in ladder {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(level.size);
        cost += take * level.price;
        remaining -= take;
    }
    cost
}

/// Taker fee to take `qty` contracts, rounded up once per level consumed.
///
/// Venue schedules round up per fill, so charging per level reproduces what the
/// exchange actually does and, where it differs, always overstates. Overstating
/// can only suppress a marginal signal; understating puts on a losing trade that
/// looked profitable. That asymmetry is the whole reason the rounding is here
/// rather than applied once to a blended average price.
pub fn apply_fees_per_level(ladder: &[Level], qty: i64, fees: &dyn FeeModel) -> Cents {
    let mut remaining = qty.max(0);
    let mut fee = 0;
    for level in ladder {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(level.size);
        fee += fees.taker_fee(level.price, take);
        remaining -= take;
    }
    fee
}

/// Worst-case payoff per unit over every resolution the relation permits.
///
/// This is the central safety check, and it is a computation rather than an
/// assumption. Each fast path believes it produces a position paying at least
/// $1 in every state; this enumerates the states and confirms it. A relation
/// whose worst case is zero produces no trade no matter how attractive the
/// prices look, because such a position is a directional bet, not an arbitrage.
pub fn min_payoff_per_unit(relation: &Relation, legs: &[CandidateLeg]) -> Cents {
    relation
        .resolution_states()
        .iter()
        .map(|state| {
            legs.iter()
                .map(|leg| {
                    let resolves_yes = state.contains(&leg.contract_id);
                    let pays = match leg.side {
                        Side::Yes => resolves_yes,
                        Side::No => !resolves_yes,
                    };
                    if pays { PAYOFF_PER_CONTRACT_CENTS } else { 0 }
                })
                .sum::<Cents>()
        })
        .min()
        .unwrap_or(0)
}

/// Quantities worth evaluating: every level boundary, plus the cap itself.
///
/// Within a single level the price per contract is fixed and the fee per
/// contract only falls (a ceiling amortizes as quantity grows), so net profit
/// rises monotonically inside a level and can only turn over where some leg
/// steps to a worse price. Checking the boundaries is therefore exact, not a
/// heuristic, and there are at most a hundred of them per side.
fn size_breakpoints(ladders: &[Vec<Level>], cap: i64) -> Vec<i64> {
    let mut points = Vec::new();
    for ladder in ladders {
        let mut cumulative = 0i64;
        for level in ladder {
            cumulative += level.size;
            if cumulative >= cap {
                break;
            }
            points.push(cumulative);
        }
    }
    points.push(cap);
    points.retain(|q| *q > 0);
    points.sort_unstable();
    points.dedup();
    points
}

/// Price a candidate against real depth, real fees, and the clock.
///
/// Returns the trade the engine would actually place, or the single concrete
/// reason it will not.
pub fn cost_candidate<B: BookSource + ?Sized>(
    group: &ConstraintGroup,
    candidate: &Candidate,
    books: &B,
    fees: &FeeModels,
    config: &SolverConfig,
    now_ms: u64,
) -> Result<Opportunity, RejectReason> {
    let payoff_per_unit = min_payoff_per_unit(&group.relation, &candidate.legs);
    if payoff_per_unit <= 0 {
        return Err(RejectReason::PayoffNotGuaranteed);
    }

    // Walk every leg once, to the position cap. Later quantities are prefixes of
    // these ladders, so depth is read from the book exactly one time per leg.
    let mut ladders: Vec<Vec<Level>> = Vec::with_capacity(candidate.legs.len());
    let mut cap = config.max_position_size.max(0);
    for leg in &candidate.legs {
        let Some(book) = books.book(leg.contract_id) else {
            return Err(RejectReason::MissingBook);
        };
        let walk = walk_depth_and_cost(book, leg.side, config.max_position_size);
        // Legs must be equal size or the position no longer settles flat, so
        // surplus depth on a deeper leg is unusable. The thinnest leg is the cap.
        cap = cap.min(walk.filled);
        ladders.push(walk.consumed);
    }
    if cap <= 0 {
        return Err(RejectReason::NoDepth);
    }

    let mut best: Option<(i64, Cents, Cents, Cents)> = None;
    for qty in size_breakpoints(&ladders, cap) {
        let mut cost = 0;
        let mut fee = 0;
        for (leg, ladder) in candidate.legs.iter().zip(&ladders) {
            cost += total_cost_cents(ladder, qty);
            fee += apply_fees_per_level(ladder, qty, fees.for_venue(leg.venue));
        }
        let net = payoff_per_unit * qty - cost - fee;
        if best.is_none_or(|(_, _, _, best_net)| net > best_net) {
            best = Some((qty, cost, fee, net));
        }
    }

    let Some((qty, total_cost, total_fees, net_cents)) = best else {
        return Err(RejectReason::NoDepth);
    };
    let guaranteed_payoff_cents = payoff_per_unit * qty;
    let gross_cents = guaranteed_payoff_cents - total_cost;
    if net_cents <= 0 {
        // Gross is positive by construction here: the fast path only fires when
        // the top-of-book legs cost less than the guaranteed payoff, and the
        // cheapest evaluated quantity pays exactly those prices. So a
        // non-positive net is always the fee schedule, never the prices.
        return Err(RejectReason::FeesExceedGap);
    }
    if net_cents < config.min_net_edge_cents {
        return Err(RejectReason::BelowMinEdge);
    }

    let capital_cents = total_cost + total_fees;
    let days_to_resolution = days_to_resolution(group.resolves_at_ms, now_ms);
    let annualized_return =
        (net_cents as f64 / capital_cents as f64) * (365.0 / days_to_resolution);
    if annualized_return < config.min_annualized_return {
        return Err(RejectReason::BelowMinReturn);
    }

    let legs = candidate
        .legs
        .iter()
        .zip(&ladders)
        .map(|(leg, ladder)| Leg {
            venue: leg.venue,
            contract_id: leg.contract_id,
            side: leg.side,
            qty,
            total_cost_cents: total_cost_cents(ladder, qty),
            fee_cents: apply_fees_per_level(ladder, qty, fees.for_venue(leg.venue)),
        })
        .collect();

    Ok(Opportunity {
        group: group.id,
        legs,
        qty,
        guaranteed_payoff_cents,
        capital_cents,
        gross_cents,
        fees_cents: total_fees,
        net_cents,
        days_to_resolution,
        annualized_return,
    })
}

/// Days until the locked capital comes back, floored at [`MIN_HORIZON_DAYS`].
fn days_to_resolution(resolves_at_ms: u64, now_ms: u64) -> f64 {
    let remaining_ms = resolves_at_ms.saturating_sub(now_ms) as f64;
    (remaining_ms / MS_PER_DAY).max(MIN_HORIZON_DAYS)
}

/// Worst price paid when taking `qty` from a walked ladder.
///
/// This, not the blended average, is the limit price a leg must carry. A limit
/// at the average would have the venue refuse the deeper half of the very fill
/// the solver costed, turning a priced trade into a partial one.
pub fn worst_price_cents(ladder: &[Level], qty: i64) -> Option<Cents> {
    let mut remaining = qty.max(0);
    let mut worst = None;
    for level in ladder {
        if remaining == 0 {
            break;
        }
        remaining -= remaining.min(level.size);
        worst = Some(level.price);
    }
    worst
}

/// Rank by annualized return on locked capital, best first.
///
/// Capital in a prediction market is trapped until the event resolves, so a one
/// cent edge locking $50 for eight months and a two cent edge locking $500 for a
/// day are not comparable on raw profit. Ties break on net profit and then on
/// group id, so the same input always produces the same order under replay.
pub fn rank(opportunities: &mut [Opportunity]) {
    opportunities.sort_by(|a, b| {
        b.annualized_return
            .total_cmp(&a.annualized_return)
            .then(b.net_cents.cmp(&a.net_cents))
            .then(a.group.cmp(&b.group))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fees::KalshiFees;
    use crate::registry::GroupId;
    use crate::types::{ContractId, Venue};

    fn ladder(levels: &[(Cents, i64)]) -> Vec<Level> {
        levels
            .iter()
            .map(|(price, size)| Level {
                price: *price,
                size: *size,
            })
            .collect()
    }

    #[test]
    fn depth_walk_pays_each_level_not_the_top_price() {
        let mut book = Book::new(Venue::Kalshi, ContractId(0));
        // Yes asks at 40, 42, 45 are no bids at 60, 58, 55.
        book.apply_snapshot(&[], &ladder(&[(60, 10), (58, 20), (55, 100)]), 1, 1_000)
            .unwrap();

        let walk = walk_depth_and_cost(&book, Side::Yes, 50);
        assert_eq!(walk.filled, 50);
        assert_eq!(walk.consumed, ladder(&[(40, 10), (42, 20), (45, 20)]));
        // Top of book times quantity would have said 2000; the truth is higher.
        assert_eq!(walk.total_cost_cents, 400 + 840 + 900);
        assert_eq!(total_cost_cents(&walk.consumed, 50), walk.total_cost_cents);
        // A prefix is priced at the levels it actually reaches.
        assert_eq!(total_cost_cents(&walk.consumed, 15), 10 * 40 + 5 * 42);
        // Requesting more than the book holds fills what exists, no more.
        assert_eq!(walk_depth_and_cost(&book, Side::Yes, 500).filled, 130);
    }

    #[test]
    fn fees_are_charged_once_per_level_and_never_understate() {
        let fees = KalshiFees::default();
        let consumed = ladder(&[(40, 10), (42, 20)]);
        let per_level = apply_fees_per_level(&consumed, 30, &fees);
        assert_eq!(per_level, fees.taker_fee(40, 10) + fees.taker_fee(42, 20));
        // Blending to an average price and rounding once would undercharge.
        let blended_price = (10 * 40 + 20 * 42) / 30;
        assert!(per_level >= fees.taker_fee(blended_price, 30));
    }

    #[test]
    fn min_payoff_is_computed_from_the_relation_not_assumed() {
        let legs = |pairs: &[(u32, Side)]| -> Vec<CandidateLeg> {
            pairs
                .iter()
                .map(|(id, side)| CandidateLeg {
                    venue: Venue::Kalshi,
                    contract_id: ContractId(*id),
                    side: *side,
                })
                .collect()
        };
        let exhaustive = Relation::Exhaustive {
            members: vec![ContractId(0), ContractId(1), ContractId(2)],
        };
        assert_eq!(
            min_payoff_per_unit(
                &exhaustive,
                &legs(&[(0, Side::Yes), (1, Side::Yes), (2, Side::Yes)])
            ),
            100
        );
        // Drop a leg and one state pays nothing: not an arbitrage at any price.
        assert_eq!(
            min_payoff_per_unit(&exhaustive, &legs(&[(0, Side::Yes), (1, Side::Yes)])),
            0
        );
        // The ladder trade pays 100 in two states and 200 in the middle one;
        // only the worst case may be counted.
        let ladder_relation = Relation::Monotone {
            ordered: vec![ContractId(0), ContractId(1)],
        };
        assert_eq!(
            min_payoff_per_unit(&ladder_relation, &legs(&[(0, Side::Yes), (1, Side::No)])),
            100
        );
        // The reversed ladder trade pays nothing when only the weak rung hits.
        assert_eq!(
            min_payoff_per_unit(&ladder_relation, &legs(&[(1, Side::Yes), (0, Side::No)])),
            0
        );
    }

    #[test]
    fn breakpoints_are_level_boundaries_capped_at_the_thinnest_leg() {
        let ladders = vec![ladder(&[(40, 10), (42, 90)]), ladder(&[(50, 30), (52, 70)])];
        assert_eq!(size_breakpoints(&ladders, 60), vec![10, 30, 60]);
        // A cap below every boundary still evaluates the cap itself.
        assert_eq!(size_breakpoints(&ladders, 5), vec![5]);
    }

    #[test]
    fn ranking_prefers_the_shorter_lockup_at_equal_profit() {
        let base = Opportunity {
            group: GroupId(0),
            legs: Vec::new(),
            qty: 1,
            guaranteed_payoff_cents: 100,
            capital_cents: 5_000,
            gross_cents: 1,
            fees_cents: 0,
            net_cents: 1,
            days_to_resolution: 240.0,
            annualized_return: (1.0 / 5_000.0) * (365.0 / 240.0),
        };
        let quick = Opportunity {
            group: GroupId(1),
            capital_cents: 50_000,
            net_cents: 2,
            days_to_resolution: 1.0,
            annualized_return: (2.0 / 50_000.0) * 365.0,
            ..base.clone()
        };
        let mut ranked = vec![base.clone(), quick.clone()];
        rank(&mut ranked);
        // 2 cents on $500 for a day is 1.46x; 1 cent on $50 for eight months is
        // 0.30x. Raw profit says the opposite, which is why ranking is on return.
        assert_eq!(ranked[0].group, quick.group);
        assert_eq!(ranked[1].group, base.group);
    }

    #[test]
    fn horizon_is_floored_so_imminent_resolution_cannot_dominate() {
        assert_eq!(days_to_resolution(1_000 + 86_400_000, 1_000), 1.0);
        // Resolution inside the hour, or already past, clamps to the floor.
        assert_eq!(days_to_resolution(1_000, 1_000), MIN_HORIZON_DAYS);
        assert_eq!(days_to_resolution(0, 1_000), MIN_HORIZON_DAYS);
    }
}
