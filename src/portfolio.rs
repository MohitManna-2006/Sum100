//! Positions, capital, and profit and loss.
//!
//! Capital is the scarce resource. A prediction market position is money locked
//! until the event settles, so the portfolio's job is to know, at every instant,
//! how much is committed, to what, and what it is currently worth.
//!
//! Every quantity is integer [`Cents`]. Marking to market is deliberately
//! pessimistic: a position is valued at what it could be *sold* for right now,
//! which is the bid, not the mid and not the ask. Valuing at the mid would book
//! an unrealized profit that the spread would take back on exit.

use crate::{
    registry::EventId,
    types::{Book, Cents, ContractId, Side},
};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PositionLeg {
    pub contract_id: ContractId,
    /// The outcome held. Both legs of an arbitrage are long positions in
    /// opposing outcomes; nothing here is short.
    pub side: Side,
    pub quantity: i64,
    pub average_entry_price: Cents,
    pub order_ids: Vec<String>,
}

impl PositionLeg {
    /// Premium paid for this leg, excluding fees.
    pub fn cost_cents(&self) -> Cents {
        self.quantity * self.average_entry_price
    }

    /// What this leg pays if the event resolves yes.
    fn payoff_cents(&self, resolved_yes: bool) -> Cents {
        let pays = match self.side {
            Side::Yes => resolved_yes,
            Side::No => !resolved_yes,
        };
        if pays { self.quantity * 100 } else { 0 }
    }

    /// What this leg could be liquidated for right now, at the bid.
    fn liquidation_cents(&self, book: &Book) -> Cents {
        let best = match self.side {
            // Selling a yes means hitting the resting yes bid; selling a no
            // means hitting the resting no bid.
            Side::Yes => book.best_bid(),
            Side::No => book.best_no_bid(),
        };
        match best {
            Some(level) => self.quantity * level.price,
            // No bid at all means no exit at any price, so the honest mark is
            // zero rather than the last trade or the other side's quote.
            None => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Position {
    pub event_id: EventId,
    pub group: u32,
    pub legs: Vec<PositionLeg>,
    /// Capital locked: premium plus the fees paid to enter.
    pub cost_cents: Cents,
    /// Fees paid on entry, already included in `cost_cents`.
    pub fees_cents: Cents,
    pub entry_time_ms: u64,
    pub resolved: bool,
    pub pnl_realized_cents: Cents,
    pub pnl_unrealized_cents: Cents,
}

impl Position {
    /// Payoff in a given resolution, minus what it cost to get in.
    pub fn pnl_at_resolution(&self, resolved_yes: bool) -> Cents {
        let payoff: Cents = self
            .legs
            .iter()
            .map(|leg| leg.payoff_cents(resolved_yes))
            .sum();
        payoff - self.cost_cents
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioError {
    /// Not enough uncommitted capital to pay for the position.
    InsufficientCapital { needed: Cents, available: Cents },
    /// Today's realized losses have reached the limit; the day is closed.
    DailyLossExceeded { realized: Cents, limit: Cents },
    /// The event is not in the portfolio.
    UnknownEvent,
}

impl std::fmt::Display for PortfolioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortfolioError::InsufficientCapital { needed, available } => {
                write!(f, "position needs {needed} cents, {available} available")
            }
            PortfolioError::DailyLossExceeded { realized, limit } => {
                write!(
                    f,
                    "daily loss {realized} cents has reached the {limit} limit"
                )
            }
            PortfolioError::UnknownEvent => write!(f, "event is not held"),
        }
    }
}

impl std::error::Error for PortfolioError {}

/// A day's worth of realized loss and the capital behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Portfolio {
    /// Cash not currently committed to a position.
    pub capital_available_cents: Cents,
    pub positions: Vec<Position>,
    pub daily_loss_limit_cents: Cents,
    /// Realized losses today, as a positive number. Profits do not offset it:
    /// the limit exists to stop a bad day, and letting a win earlier in the
    /// session buy back room to lose is how a bad day becomes a worse one.
    pub daily_loss_realized_cents: Cents,
    /// Next UTC midnight, in epoch milliseconds.
    pub daily_reset_at_ms: u64,
    pub pnl_realized_cents: Cents,
}

/// Milliseconds in a day.
const DAY_MS: u64 = 86_400_000;

/// The next UTC midnight strictly after `now_ms`.
///
/// Calendar midnight, not "24 hours from whenever the process started": a loss
/// limit that resets at a different wall-clock time on every restart is not a
/// daily limit, and two restarts in one day would hand out two fresh budgets.
pub fn next_utc_midnight_ms(now_ms: u64) -> u64 {
    (now_ms / DAY_MS + 1) * DAY_MS
}

impl Portfolio {
    pub fn new(starting_capital_cents: Cents, daily_loss_limit_cents: Cents, now_ms: u64) -> Self {
        Portfolio {
            capital_available_cents: starting_capital_cents,
            positions: Vec::new(),
            daily_loss_limit_cents,
            daily_loss_realized_cents: 0,
            daily_reset_at_ms: next_utc_midnight_ms(now_ms),
            pnl_realized_cents: 0,
        }
    }

    pub fn can_afford(&self, cost_cents: Cents) -> bool {
        self.capital_available_cents >= cost_cents
    }

    /// Whether the day is still open for new risk.
    pub fn within_daily_loss_limit(&self) -> bool {
        self.daily_loss_realized_cents < self.daily_loss_limit_cents
    }

    /// Commit capital and take on a position.
    ///
    /// Capital moves out of `capital_available_cents` here and comes back only
    /// at resolution. The two checks are the last gate before money is at risk,
    /// so they are enforced on the way in rather than assumed by the caller.
    pub fn add_position(&mut self, position: Position) -> Result<(), PortfolioError> {
        if !self.within_daily_loss_limit() {
            return Err(PortfolioError::DailyLossExceeded {
                realized: self.daily_loss_realized_cents,
                limit: self.daily_loss_limit_cents,
            });
        }
        if !self.can_afford(position.cost_cents) {
            return Err(PortfolioError::InsufficientCapital {
                needed: position.cost_cents,
                available: self.capital_available_cents,
            });
        }
        self.capital_available_cents -= position.cost_cents;
        self.positions.push(position);
        Ok(())
    }

    /// Exposure to one event, as capital currently locked in it.
    pub fn exposure_to(&self, event: EventId) -> Cents {
        self.positions
            .iter()
            .filter(|p| !p.resolved && p.event_id == event)
            .map(|p| p.cost_cents)
            .sum()
    }

    pub fn open_positions(&self) -> impl Iterator<Item = &Position> {
        self.positions.iter().filter(|p| !p.resolved)
    }

    pub fn open_count(&self) -> usize {
        self.open_positions().count()
    }

    pub fn pnl_unrealized_cents(&self) -> Cents {
        self.open_positions().map(|p| p.pnl_unrealized_cents).sum()
    }

    /// Revalue open positions against current books, at the bid.
    ///
    /// `books` is looked up per leg; a leg whose book is missing is marked at
    /// zero, which understates the position rather than inventing a value for
    /// something the engine cannot currently see.
    pub fn mark_to_market<'a, F>(&mut self, book_for: F)
    where
        F: Fn(ContractId) -> Option<&'a Book>,
    {
        for position in self.positions.iter_mut().filter(|p| !p.resolved) {
            let liquidation: Cents = position
                .legs
                .iter()
                .map(|leg| match book_for(leg.contract_id) {
                    Some(book) => leg.liquidation_cents(book),
                    None => 0,
                })
                .sum();
            position.pnl_unrealized_cents = liquidation - position.cost_cents;
        }
    }

    /// Settle every open position on an event and return the realized profit.
    ///
    /// Capital comes back as the payoff, not as the original cost: that is what
    /// actually lands in the account when the venue settles.
    pub fn realize(&mut self, event: EventId, resolved_yes: bool) -> Result<Cents, PortfolioError> {
        let mut total = 0;
        let mut found = false;
        for position in self.positions.iter_mut() {
            if position.resolved || position.event_id != event {
                continue;
            }
            found = true;
            let pnl = position.pnl_at_resolution(resolved_yes);
            position.resolved = true;
            position.pnl_realized_cents = pnl;
            position.pnl_unrealized_cents = 0;
            self.capital_available_cents += position.cost_cents + pnl;
            self.pnl_realized_cents += pnl;
            if pnl < 0 {
                self.daily_loss_realized_cents += -pnl;
            }
            total += pnl;
        }
        if !found {
            return Err(PortfolioError::UnknownEvent);
        }
        Ok(total)
    }

    /// Clear the day's loss budget once the calendar day has turned over.
    pub fn reset_daily_if_needed(&mut self, now_ms: u64) -> bool {
        if now_ms < self.daily_reset_at_ms {
            return false;
        }
        self.daily_loss_realized_cents = 0;
        self.daily_reset_at_ms = next_utc_midnight_ms(now_ms);
        true
    }

    pub fn summary(&self) -> PortfolioSummary {
        PortfolioSummary {
            capital_available_cents: self.capital_available_cents,
            capital_locked_cents: self.open_positions().map(|p| p.cost_cents).sum(),
            open_positions: self.open_count(),
            pnl_realized_cents: self.pnl_realized_cents,
            pnl_unrealized_cents: self.pnl_unrealized_cents(),
            daily_loss_cents: self.daily_loss_realized_cents,
            daily_loss_limit_cents: self.daily_loss_limit_cents,
        }
    }
}

/// What the API layer publishes about the portfolio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PortfolioSummary {
    pub capital_available_cents: Cents,
    pub capital_locked_cents: Cents,
    pub open_positions: usize,
    pub pnl_realized_cents: Cents,
    pub pnl_unrealized_cents: Cents,
    pub daily_loss_cents: Cents,
    pub daily_loss_limit_cents: Cents,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Level, Venue};

    const NOW: u64 = 1_789_343_120_404;

    fn leg(contract: u32, side: Side, quantity: i64, price: Cents) -> PositionLeg {
        PositionLeg {
            contract_id: ContractId(contract),
            side,
            quantity,
            average_entry_price: price,
            order_ids: vec![format!("order-{contract}")],
        }
    }

    /// A complement position: 100 yes at 55 and 100 no at 40, 683 cents of fees.
    fn complement_position() -> Position {
        Position {
            event_id: EventId(0),
            group: 0,
            legs: vec![leg(0, Side::Yes, 100, 55), leg(0, Side::No, 100, 40)],
            cost_cents: 9_500 + 683,
            fees_cents: 683,
            entry_time_ms: NOW,
            resolved: false,
            pnl_realized_cents: 0,
            pnl_unrealized_cents: 0,
        }
    }

    fn book_quoting(bid_yes: Cents, bid_no: Cents) -> Book {
        let mut book = Book::new(Venue::Kalshi, ContractId(0));
        book.apply_snapshot(
            &[Level {
                price: bid_yes,
                size: 500,
            }],
            &[Level {
                price: bid_no,
                size: 500,
            }],
            1,
            NOW,
        )
        .unwrap();
        book
    }

    #[test]
    fn capital_is_committed_on_entry_and_returned_at_settlement() {
        let mut portfolio = Portfolio::new(50_000, 10_000, NOW);
        portfolio.add_position(complement_position()).unwrap();
        assert_eq!(portfolio.capital_available_cents, 50_000 - 10_183);
        assert_eq!(portfolio.open_count(), 1);
        assert_eq!(portfolio.exposure_to(EventId(0)), 10_183);

        // Exactly one of the two legs pays a dollar, whichever way it resolves.
        let pnl = portfolio.realize(EventId(0), true).unwrap();
        assert_eq!(pnl, 10_000 - 10_183);
        assert_eq!(portfolio.capital_available_cents, 50_000 - 183);
        assert_eq!(portfolio.pnl_realized_cents, -183);
        assert_eq!(portfolio.open_count(), 0);
        assert_eq!(portfolio.exposure_to(EventId(0)), 0);
    }

    #[test]
    fn a_position_that_cannot_be_paid_for_is_refused() {
        let mut portfolio = Portfolio::new(5_000, 10_000, NOW);
        assert_eq!(
            portfolio.add_position(complement_position()),
            Err(PortfolioError::InsufficientCapital {
                needed: 10_183,
                available: 5_000,
            })
        );
        // Nothing was committed by the failed attempt.
        assert_eq!(portfolio.capital_available_cents, 5_000);
        assert!(portfolio.positions.is_empty());
    }

    #[test]
    fn the_day_closes_once_realized_losses_reach_the_limit() {
        let mut portfolio = Portfolio::new(100_000, 200, NOW);
        portfolio.add_position(complement_position()).unwrap();
        // Settling at a 183 cent loss does not yet reach a 200 cent limit.
        portfolio.realize(EventId(0), false).unwrap();
        assert_eq!(portfolio.daily_loss_realized_cents, 183);
        assert!(portfolio.within_daily_loss_limit());
        portfolio.add_position(complement_position()).unwrap();

        portfolio.realize(EventId(0), false).unwrap();
        assert_eq!(portfolio.daily_loss_realized_cents, 366);
        assert!(!portfolio.within_daily_loss_limit());
        assert_eq!(
            portfolio.add_position(complement_position()),
            Err(PortfolioError::DailyLossExceeded {
                realized: 366,
                limit: 200,
            })
        );
    }

    #[test]
    fn the_loss_budget_resets_at_calendar_midnight_not_after_24_hours() {
        let mut portfolio = Portfolio::new(100_000, 200, NOW);
        portfolio.daily_loss_realized_cents = 500;
        let midnight = portfolio.daily_reset_at_ms;
        assert_eq!(midnight % DAY_MS, 0, "reset lands on a UTC day boundary");
        assert!(midnight > NOW && midnight - NOW <= DAY_MS);

        assert!(!portfolio.reset_daily_if_needed(midnight - 1));
        assert_eq!(portfolio.daily_loss_realized_cents, 500);
        assert!(portfolio.reset_daily_if_needed(midnight));
        assert_eq!(portfolio.daily_loss_realized_cents, 0);
        assert_eq!(portfolio.daily_reset_at_ms, midnight + DAY_MS);
    }

    #[test]
    fn positions_mark_at_the_bid_not_the_mid() {
        let mut portfolio = Portfolio::new(50_000, 10_000, NOW);
        portfolio.add_position(complement_position()).unwrap();

        // Yes bids 54 and no bids 44: liquidating both legs returns 9800
        // against 10183 committed.
        let book = book_quoting(54, 44);
        portfolio.mark_to_market(|_| Some(&book));
        assert_eq!(portfolio.pnl_unrealized_cents(), 9_800 - 10_183);

        // The asks are 56 and 46, which would have shown a profit. Marking
        // there would book money the spread takes back on exit.
        assert!(portfolio.pnl_unrealized_cents() < 0);

        // A book with no bid at all marks that leg at zero, not at its cost.
        let empty = Book::new(Venue::Kalshi, ContractId(0));
        portfolio.mark_to_market(|_| Some(&empty));
        assert_eq!(portfolio.pnl_unrealized_cents(), -10_183);

        // A missing book is the same story: understate, never invent.
        portfolio.mark_to_market(|_| None);
        assert_eq!(portfolio.pnl_unrealized_cents(), -10_183);
    }

    #[test]
    fn a_settled_position_stops_being_marked_or_counted() {
        let mut portfolio = Portfolio::new(50_000, 10_000, NOW);
        portfolio.add_position(complement_position()).unwrap();
        portfolio.realize(EventId(0), true).unwrap();
        let book = book_quoting(54, 44);
        portfolio.mark_to_market(|_| Some(&book));
        assert_eq!(portfolio.pnl_unrealized_cents(), 0);
        assert_eq!(portfolio.summary().capital_locked_cents, 0);
        assert_eq!(
            portfolio.realize(EventId(0), true),
            Err(PortfolioError::UnknownEvent)
        );
    }
}
