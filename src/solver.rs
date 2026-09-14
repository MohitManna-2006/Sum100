//! Coherence solver.
//!
//! A mutually exclusive and exhaustive set of prediction market contracts must
//! settle with exactly one leg paying $1. If every leg can be bought for a
//! combined total below 100 cents, the set is incoherent and the difference is
//! locked in regardless of which outcome occurs. This module turns that
//! observation into an executable, fee-aware, depth-aware signal.
//!
//! All money is integer [`Cents`]. Binary contract prices are already quoted in
//! whole cents, so integers are exact: they cannot drift, cannot accumulate
//! representation error across a multi-leg sum, and cannot turn a break-even
//! set into a phantom edge through a rounding artifact. The only float in this
//! module is [`Opportunity::annualized_return`], which exists purely to rank
//! and display candidates that have already passed an integer go/no-go test.

use crate::fees::FeeModel;
use crate::types::{Book, Cents, ContractId, Venue};

/// Result of consuming resting ask liquidity in a single book.
///
/// Depth matters as much as the top-of-book price: an apparent edge that only
/// exists for 3 contracts is not a trade. Capturing `filled` separately from
/// the requested size lets the caller discover the real executable size before
/// committing to any leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Walk {
    /// Contracts actually obtainable, which may be less than requested.
    pub filled: i64,
    /// Total premium paid, as size times price summed across levels.
    pub cost: Cents,
    /// Total taker fee, rounded up once per level consumed.
    pub fee: Cents,
}

/// Walk the ask side from cheapest upward, buying up to `want` contracts.
///
/// Fees are accumulated per level rather than once on the blended average.
/// Venue fee schedules round up per fill, so per-level rounding reproduces what
/// the exchange actually charges and, where it differs, always overstates the
/// cost. Overstating cost can only suppress a marginal signal, never invent
/// one, which is the direction an arbitrage engine must err in.
pub fn walk_asks(book: &Book, want: i64, fees: &dyn FeeModel) -> Walk {
    let mut walk = Walk {
        filled: 0,
        cost: 0,
        fee: 0,
    };
    if want <= 0 {
        return walk;
    }
    for level in book.asks() {
        let remaining = want - walk.filled;
        if remaining <= 0 {
            break;
        }
        let take = remaining.min(level.size);
        if take <= 0 {
            continue;
        }
        walk.filled += take;
        walk.cost += take * level.price;
        walk.fee += fees.taker_fee(level.price, take);
    }
    walk
}

/// One venue-side fill that forms part of a combined position.
///
/// Legs are retained individually because execution is per venue and per
/// contract: the engine must be able to place, size, and later reconcile each
/// one on its own, and a post-mortem needs to know which leg moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    /// Venue this leg executes on.
    pub venue: Venue,
    /// Venue-scoped contract identifier.
    pub contract_id: ContractId,
    /// Contracts bought on this leg.
    pub qty: i64,
    /// Premium paid on this leg.
    pub cost: Cents,
    /// Taker fee charged on this leg.
    pub fee: Cents,
}

/// A fully priced, fully sized incoherence that is profitable after costs.
///
/// Every field is the post-fee, post-depth truth rather than a headline number,
/// so downstream ranking and risk checks never have to re-derive costs and
/// never disagree with each other about what the trade is worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opportunity {
    /// The individual fills that make up the position.
    pub legs: Vec<Leg>,
    /// Contracts held on every leg; the set only settles flat if legs match.
    pub qty: i64,
    /// Cash locked until resolution, which is what the return is measured on.
    pub capital: Cents,
    /// Payoff minus premium, before fees.
    pub gross: Cents,
    /// Total taker fees across all legs.
    pub fees: Cents,
    /// Profit actually realized at resolution.
    pub net: Cents,
}

impl Opportunity {
    /// Return on locked capital, annualized over the time to resolution.
    ///
    /// Capital in a prediction market is trapped until the event resolves, so a
    /// 2% edge that unlocks next week and a 2% edge that unlocks next year are
    /// not the same trade. Annualizing puts positions of different tenors on one
    /// comparable axis. This is a float because it is a ratio for ranking and
    /// display only; the profitability decision is made on integer cents before
    /// this is ever called.
    pub fn annualized_return(&self, days_to_resolution: f64) -> f64 {
        if self.capital <= 0 || days_to_resolution <= 0.0 {
            return 0.0;
        }
        (self.net as f64 / self.capital as f64) * (365.0 / days_to_resolution)
    }
}

/// Scan a mutually exclusive and exhaustive set for a risk-free underpricing.
///
/// Returns `Some` only when the whole set can be bought at a size that clears a
/// strictly positive net profit after real depth and real fees. Any doubt —
/// too few legs, a stale quote, insufficient depth, a non-positive net —
/// produces `None`, because a false negative costs an opportunity while a false
/// positive costs money.
pub fn scan_exhaustive_set(
    books: &[Book],
    fees: &dyn FeeModel,
    max_qty: i64,
    now_ms: u64,
    max_age_ms: u64,
) -> Option<Opportunity> {
    if books.len() < 2 || max_qty <= 0 {
        return None;
    }

    // A single stale leg invalidates the entire set: the arithmetic only holds
    // if every price is simultaneously live, and acting on a quote that has
    // already moved is how a "risk-free" position becomes a directional loss.
    if !books.iter().all(|b| b.is_fresh(now_ms, max_age_ms)) {
        return None;
    }

    // Size the position to the thinnest leg. Legs must be equal size or the set
    // no longer settles flat, so surplus depth elsewhere is unusable.
    let mut qty = max_qty;
    for book in books {
        let fill = walk_asks(book, max_qty, fees).filled;
        if fill < qty {
            qty = fill;
        }
    }
    if qty <= 0 {
        return None;
    }

    let mut legs = Vec::with_capacity(books.len());
    let mut total_cost: Cents = 0;
    let mut total_fees: Cents = 0;
    for book in books {
        let walk = walk_asks(book, qty, fees);
        total_cost += walk.cost;
        total_fees += walk.fee;
        legs.push(Leg {
            venue: book.venue,
            contract_id: book.contract_id,
            qty,
            cost: walk.cost,
            fee: walk.fee,
        });
    }

    // Exactly one outcome in an exhaustive set resolves yes, paying $1 per
    // contract, so the payoff is known with certainty up front.
    let payoff = qty * 100;
    let gross = payoff - total_cost;
    let net = gross - total_fees;
    if net <= 0 {
        return None;
    }

    Some(Opportunity {
        legs,
        qty,
        capital: total_cost + total_fees,
        gross,
        fees: total_fees,
        net,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fees::KalshiFees;
    use crate::types::{BookState, Level};

    /// Single-level Kalshi book via a no-side bid at `100 - price` (yes ask).
    fn book(contract_id: u32, price: Cents, size: i64) -> Book {
        let mut b = Book::new(Venue::Kalshi, ContractId(contract_id));
        b.apply_snapshot(
            &[],
            &[Level {
                price: 100 - price,
                size,
            }],
            1,
            1000,
        )
        .unwrap();
        assert_eq!(b.state, BookState::Live);
        assert_eq!(b.best_ask().map(|l| (l.price, l.size)), Some((price, size)));
        b
    }

    #[test]
    fn two_cent_gap_is_eaten_by_fees() {
        let prices = [4, 62, 29, 3];
        assert_eq!(prices.iter().sum::<Cents>(), 98);

        let fees = KalshiFees::default();
        let total_fee: Cents = prices.iter().map(|&p| fees.taker_fee(p, 100)).sum();
        assert_eq!(total_fee, 358);

        let books: Vec<Book> = prices
            .iter()
            .enumerate()
            .map(|(i, &p)| book(i as u32, p, 100))
            .collect();
        assert_eq!(scan_exhaustive_set(&books, &fees, 100, 1000, 500), None);
    }

    #[test]
    fn five_cent_gap_survives_fees() {
        let prices = [3, 60, 29, 3];
        assert_eq!(prices.iter().sum::<Cents>(), 95);

        let fees = KalshiFees::default();
        let books: Vec<Book> = prices
            .iter()
            .enumerate()
            .map(|(i, &p)| book(i as u32, p, 100))
            .collect();

        let opp =
            scan_exhaustive_set(&books, &fees, 100, 1000, 500).expect("5 cent gap must clear fees");
        assert_eq!(opp.gross, 500);
        assert!(opp.net > 0);
        assert_eq!(opp.legs.len(), 4);
    }

    #[test]
    fn stale_leg_is_rejected() {
        let prices = [3, 60, 29, 3];
        let fees = KalshiFees::default();
        let mut books: Vec<Book> = prices
            .iter()
            .enumerate()
            .map(|(i, &p)| book(i as u32, p, 100))
            .collect();
        books[0].updated_at_ms = 0;

        assert_eq!(scan_exhaustive_set(&books, &fees, 100, 1000, 500), None);
    }
}
