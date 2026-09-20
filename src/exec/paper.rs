//! Simulated fills against a real book.
//!
//! Paper mode exists to answer one question honestly: if this had been live,
//! what would have happened? That means filling the way the venue would — by
//! walking resting depth level by level and charging the real fee schedule —
//! not by assuming the whole order goes off at top of book. Filling a
//! 500-contract order at the best price would flatter every result the engine
//! ever reports, and in exactly the direction that makes a bad strategy look
//! good.
//!
//! The book snapshot is refreshed by the engine before each trade, so a paper
//! fill is priced off the same state the solver decided on.

use super::{NewOrder, OrderAction, OrderClient, OrderError, OrderFill, TimeInForce};
use crate::{
    fees::FeeModels,
    solver::costing::{apply_fees_per_level, total_cost_cents, walk_depth_and_cost},
    types::{Book, Cents, ContractId, Side},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

pub struct PaperOrderClient {
    /// Books as of the moment the engine decided to trade. A `Mutex` is fine
    /// here: execution is rare and already the slow path, and the solver's hot
    /// loop never touches this.
    books: Mutex<Vec<Book>>,
    fees: FeeModels,
    next_order_id: AtomicU64,
    now_ms: AtomicU64,
    /// Every fill this client has simulated, for the trade log.
    pub filled: Mutex<Vec<OrderFill>>,
}

impl PaperOrderClient {
    pub fn new(fees: FeeModels) -> Self {
        PaperOrderClient {
            books: Mutex::new(Vec::new()),
            fees,
            next_order_id: AtomicU64::new(1),
            now_ms: AtomicU64::new(0),
            filled: Mutex::new(Vec::new()),
        }
    }

    /// Point the simulator at the current books and clock.
    pub fn observe(&self, books: &[Book], now_ms: u64) {
        if let Ok(mut held) = self.books.lock() {
            held.clear();
            held.extend_from_slice(books);
        }
        self.now_ms.store(now_ms, Ordering::Relaxed);
    }

    fn fill(&self, order: &NewOrder) -> Result<OrderFill, OrderError> {
        let books = self
            .books
            .lock()
            .map_err(|_| OrderError::Transport("paper book snapshot poisoned".into()))?;
        let Some(book) = books
            .iter()
            .find(|book| book.contract_id == order.contract_id)
        else {
            return Err(OrderError::Rejected(format!(
                "no book for contract {}",
                order.contract_id.0
            )));
        };

        // Selling is only reached when unwinding a legged position, and that is
        // phase 7 work. Refusing is better than simulating a path the engine
        // does not yet take.
        if order.action == OrderAction::Sell {
            return Err(OrderError::Rejected(
                "paper mode does not simulate sells; unwinding is phase 7".into(),
            ));
        }

        let walk = walk_depth_and_cost(book, order.outcome, order.quantity);
        if walk.filled < order.quantity {
            // Fill-or-kill is the whole point: a partial would leave the
            // position unbalanced, which is the thing the executor exists to
            // avoid.
            if order.time_in_force == TimeInForce::FillOrKill {
                return Err(OrderError::Unfillable {
                    wanted: order.quantity,
                    available: walk.filled,
                });
            }
            if walk.filled == 0 {
                return Err(OrderError::Unfillable {
                    wanted: order.quantity,
                    available: 0,
                });
            }
        }

        let quantity = walk.filled.min(order.quantity);
        let cost = total_cost_cents(&walk.consumed, quantity);
        // The solver costed this trade at a limit price. A book that moved
        // against it between decision and placement is a miss, not a fill at a
        // worse price.
        let average = cost / quantity.max(1);
        if average > order.limit_price {
            return Err(OrderError::Rejected(format!(
                "average {average} exceeds limit {}",
                order.limit_price
            )));
        }
        let fee = apply_fees_per_level(&walk.consumed, quantity, self.fees.for_venue(book.venue));

        Ok(OrderFill {
            order_id: format!(
                "paper-{}",
                self.next_order_id.fetch_add(1, Ordering::Relaxed)
            ),
            contract_id: order.contract_id,
            outcome: order.outcome,
            action: order.action,
            quantity_filled: quantity,
            average_price: average,
            fee_cents: fee,
            timestamp_ms: self.now_ms.load(Ordering::Relaxed),
        })
    }
}

impl OrderClient for PaperOrderClient {
    fn place_order<'a>(
        &'a self,
        order: NewOrder,
    ) -> Pin<Box<dyn Future<Output = Result<OrderFill, OrderError>> + Send + 'a>> {
        let result = self.fill(&order);
        if let Ok(fill) = &result
            && let Ok(mut log) = self.filled.lock()
        {
            log.push(fill.clone());
        }
        Box::pin(async move { result })
    }

    fn cancel_order<'a>(
        &'a self,
        _order_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), OrderError>> + Send + 'a>> {
        // Nothing rests in paper mode: every order fills or dies immediately.
        Box::pin(async { Ok(()) })
    }

    fn is_live(&self) -> bool {
        false
    }

    fn observe_books(&self, books: &[Book], now_ms: u64) {
        self.observe(books, now_ms);
    }
}

/// Price per contract if `quantity` were bought right now, for display.
pub fn marketable_price(book: &Book, outcome: Side, quantity: i64) -> Option<Cents> {
    let walk = walk_depth_and_cost(book, outcome, quantity);
    (walk.filled == quantity && quantity > 0)
        .then(|| total_cost_cents(&walk.consumed, quantity) / quantity)
}

/// Helper for tests and the trade log: which book a fill belongs to.
pub fn contract_of(fill: &OrderFill) -> ContractId {
    fill.contract_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exec::TimeInForce,
        types::{Level, Venue},
    };

    fn book_with(asks: &[(Cents, i64)]) -> Book {
        let mut book = Book::new(Venue::Kalshi, ContractId(0));
        let no_bids: Vec<Level> = asks
            .iter()
            .map(|(price, size)| Level {
                price: 100 - price,
                size: *size,
            })
            .collect();
        book.apply_snapshot(&[], &no_bids, 1, 1_000).unwrap();
        book
    }

    fn buy(quantity: i64, limit: Cents, tif: TimeInForce) -> NewOrder {
        NewOrder {
            contract_id: ContractId(0),
            ticker: "T".into(),
            outcome: Side::Yes,
            action: OrderAction::Buy,
            quantity,
            limit_price: limit,
            time_in_force: tif,
            client_order_id: "c1".into(),
        }
    }

    #[tokio::test]
    async fn a_paper_fill_walks_depth_instead_of_taking_top_of_book() {
        let client = PaperOrderClient::new(FeeModels::default());
        client.observe(&[book_with(&[(40, 10), (45, 100)])], 2_000);

        let fill = client
            .place_order(buy(50, 50, TimeInForce::FillOrKill))
            .await
            .unwrap();
        // 10 at 40 plus 40 at 45 is 2200, not 50 x 40 = 2000.
        assert_eq!(fill.quantity_filled, 50);
        assert_eq!(fill.average_price, 44);
        assert_eq!(fill.cost_cents(), 44 * 50 + fill.fee_cents);
        assert!(fill.fee_cents > 0, "the real schedule is charged");
        assert_eq!(fill.timestamp_ms, 2_000);
    }

    #[tokio::test]
    async fn fill_or_kill_refuses_rather_than_leaving_half_a_position() {
        let client = PaperOrderClient::new(FeeModels::default());
        client.observe(&[book_with(&[(40, 10)])], 2_000);

        assert_eq!(
            client
                .place_order(buy(50, 50, TimeInForce::FillOrKill))
                .await,
            Err(OrderError::Unfillable {
                wanted: 50,
                available: 10,
            })
        );
        // Immediate-or-cancel accepts the partial, which is why arbitrage legs
        // never use it.
        let partial = client
            .place_order(buy(50, 50, TimeInForce::ImmediateOrCancel))
            .await
            .unwrap();
        assert_eq!(partial.quantity_filled, 10);
    }

    #[tokio::test]
    async fn a_book_that_moved_past_the_limit_is_a_miss_not_a_worse_fill() {
        let client = PaperOrderClient::new(FeeModels::default());
        client.observe(&[book_with(&[(60, 100)])], 2_000);
        let error = client
            .place_order(buy(50, 45, TimeInForce::FillOrKill))
            .await
            .unwrap_err();
        assert!(matches!(error, OrderError::Rejected(_)), "{error:?}");

        // A contract with no book at all is rejected, never silently filled.
        client.observe(&[], 2_000);
        assert!(matches!(
            client
                .place_order(buy(1, 99, TimeInForce::FillOrKill))
                .await,
            Err(OrderError::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn paper_mode_never_claims_to_be_live() {
        let client = PaperOrderClient::new(FeeModels::default());
        assert!(!client.is_live());
        assert!(client.cancel_order("paper-1").await.is_ok());
    }
}
