//! Fire every leg of a position at once, and be honest about what happened.

use super::{NewOrder, OrderClient, OrderError, OrderFill};
use futures_util::future::join_all;
use serde::Serialize;
use std::time::Duration;

/// Default window for a whole multi-leg trade.
///
/// The edge being captured exists in the book right now. Half a second is long
/// enough for two round trips to a venue and short enough that a fill arriving
/// after it is priced off a book that has moved.
pub const DEFAULT_TIMEOUT_MS: u64 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicTrade {
    pub legs: Vec<NewOrder>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionResult {
    pub fills: Vec<OrderFill>,
}

impl ExecutionResult {
    pub fn total_cost_cents(&self) -> crate::types::Cents {
        self.fills.iter().map(OrderFill::cost_cents).sum()
    }
}

/// Why a trade did not go on cleanly.
///
/// The distinction that matters is whether money is now at risk.
/// [`ExecutionError::NoFills`] is free. [`ExecutionError::Legged`] and
/// [`ExecutionError::Timeout`] both mean an unbalanced position may exist and a
/// human has to look.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExecutionError {
    /// Nothing filled. No position, no exposure, nothing to undo.
    NoFills { failed: Vec<OrderError> },
    /// Some legs filled and some did not. The fills are real and cannot be
    /// cancelled; they must be traded back out. Unwinding is phase 7.
    Legged {
        filled: Vec<OrderFill>,
        failed: Vec<OrderError>,
    },
    /// The window closed before every leg answered. Any leg may or may not have
    /// filled at the venue; the local view is not authoritative and the
    /// position must be reconciled against the venue before trading resumes.
    Timeout { elapsed_ms: u64 },
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionError::NoFills { failed } => {
                write!(f, "no legs filled ({} failure(s))", failed.len())
            }
            ExecutionError::Legged { filled, failed } => write!(
                f,
                "legged: {} leg(s) filled and must be unwound, {} failed",
                filled.len(),
                failed.len()
            ),
            ExecutionError::Timeout { elapsed_ms } => write!(
                f,
                "trade timed out after {elapsed_ms}ms; venue state unknown"
            ),
        }
    }
}

impl std::error::Error for ExecutionError {}

impl AtomicTrade {
    pub fn new(legs: Vec<NewOrder>) -> Self {
        AtomicTrade {
            legs,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    /// Place every leg concurrently and wait for all of them.
    ///
    /// Sequential placement would be strictly worse: the second leg would be
    /// priced off a book that has already seen the first, which is exactly the
    /// move that removes the edge. Concurrency here is not an optimization, it
    /// is the mechanism.
    pub async fn execute(
        &self,
        client: &dyn OrderClient,
    ) -> Result<ExecutionResult, ExecutionError> {
        if self.legs.is_empty() {
            return Ok(ExecutionResult { fills: Vec::new() });
        }
        let placements = join_all(
            self.legs
                .iter()
                .map(|leg| client.place_order(leg.clone()))
                .collect::<Vec<_>>(),
        );
        let Ok(results) =
            tokio::time::timeout(Duration::from_millis(self.timeout_ms), placements).await
        else {
            // The futures are dropped here, so whether anything reached the
            // venue is genuinely unknown. Saying so is the only honest report.
            return Err(ExecutionError::Timeout {
                elapsed_ms: self.timeout_ms,
            });
        };

        let mut fills = Vec::with_capacity(results.len());
        let mut failed = Vec::new();
        for result in results {
            match result {
                Ok(fill) => fills.push(fill),
                Err(error) => failed.push(error),
            }
        }

        if failed.is_empty() {
            // No elapsed time is logged here on purpose. Reading a monotonic
            // instant would be a clock read, and the crate keeps every one of
            // those inside `clock.rs` so replay reproduces a live run exactly.
            // The engine stamps fills from the injected clock instead.
            tracing::info!(
                legs = fills.len(),
                cost_cents = fills.iter().map(OrderFill::cost_cents).sum::<i64>(),
                "trade filled"
            );
            return Ok(ExecutionResult { fills });
        }
        if fills.is_empty() {
            tracing::warn!(failures = failed.len(), "trade placed nothing");
            return Err(ExecutionError::NoFills { failed });
        }
        // Deliberately not attempting to "cancel" the fills. They are done.
        tracing::error!(
            filled = fills.len(),
            failed = failed.len(),
            "LEGGED: filled legs are live and unbalanced; manual unwind required"
        );
        Err(ExecutionError::Legged {
            filled: fills,
            failed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exec::{OrderAction, TimeInForce},
        types::{ContractId, Side},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn order(contract: u32, quantity: i64, price: i64) -> NewOrder {
        NewOrder {
            contract_id: ContractId(contract),
            ticker: format!("T{contract}"),
            outcome: Side::Yes,
            action: OrderAction::Buy,
            quantity,
            limit_price: price,
            time_in_force: TimeInForce::FillOrKill,
            client_order_id: format!("client-{contract}"),
        }
    }

    /// Scripted client: each contract id is told to fill, fail, or stall.
    struct MockClient {
        behavior: Vec<Behavior>,
        /// Highest number of placements in flight at once.
        concurrent_peak: Arc<AtomicU64>,
        in_flight: Arc<AtomicU64>,
        cancels: Arc<AtomicU64>,
    }

    #[derive(Clone, Copy)]
    enum Behavior {
        Fills,
        Fails,
        Stalls,
    }

    impl MockClient {
        fn new(behavior: Vec<Behavior>) -> Self {
            MockClient {
                behavior,
                concurrent_peak: Arc::new(AtomicU64::new(0)),
                in_flight: Arc::new(AtomicU64::new(0)),
                cancels: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    impl OrderClient for MockClient {
        fn place_order<'a>(
            &'a self,
            order: NewOrder,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OrderFill, OrderError>> + Send + 'a>,
        > {
            let behavior = self.behavior[order.contract_id.0 as usize];
            let in_flight = self.in_flight.clone();
            let peak = self.concurrent_peak.clone();
            Box::pin(async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Yield so every leg is genuinely in flight together before any
                // of them completes.
                tokio::task::yield_now().await;
                if matches!(behavior, Behavior::Stalls) {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
                match behavior {
                    Behavior::Fills => Ok(OrderFill {
                        order_id: format!("venue-{}", order.contract_id.0),
                        contract_id: order.contract_id,
                        outcome: order.outcome,
                        action: order.action,
                        quantity_filled: order.quantity,
                        average_price: order.limit_price,
                        fee_cents: 0,
                        timestamp_ms: 1_000,
                    }),
                    Behavior::Fails => Err(OrderError::Unfillable {
                        wanted: order.quantity,
                        available: 0,
                    }),
                    Behavior::Stalls => unreachable!(),
                }
            })
        }

        fn cancel_order<'a>(
            &'a self,
            _order_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OrderError>> + Send + 'a>>
        {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn is_live(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn both_legs_go_out_at_once_not_one_after_the_other() {
        let client = MockClient::new(vec![Behavior::Fills, Behavior::Fills]);
        let trade = AtomicTrade::new(vec![order(0, 100, 45), order(1, 100, 50)]);
        let result = trade.execute(&client).await.unwrap();

        assert_eq!(result.fills.len(), 2);
        assert_eq!(result.total_cost_cents(), 100 * 45 + 100 * 50);
        // Sequential placement would never show two in flight together, and
        // would price the second leg off a book that saw the first.
        assert_eq!(client.concurrent_peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_filled_leg_beside_a_failed_one_is_reported_as_legged() {
        let client = MockClient::new(vec![Behavior::Fills, Behavior::Fails]);
        let trade = AtomicTrade::new(vec![order(0, 100, 45), order(1, 100, 50)]);
        let error = trade.execute(&client).await.unwrap_err();

        match error {
            ExecutionError::Legged { filled, failed } => {
                assert_eq!(filled.len(), 1);
                assert_eq!(filled[0].contract_id, ContractId(0));
                assert_eq!(failed.len(), 1);
            }
            other => panic!("expected Legged, got {other:?}"),
        }
        // No cancel was attempted against the fill: a filled order cannot be
        // cancelled, and pretending otherwise would hide a live position.
        assert_eq!(client.cancels.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn every_leg_failing_leaves_no_exposure() {
        let client = MockClient::new(vec![Behavior::Fails, Behavior::Fails]);
        let trade = AtomicTrade::new(vec![order(0, 100, 45), order(1, 100, 50)]);
        match trade.execute(&client).await.unwrap_err() {
            ExecutionError::NoFills { failed } => assert_eq!(failed.len(), 2),
            other => panic!("expected NoFills, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stalled_leg_closes_the_window_and_reports_unknown_state() {
        let client = MockClient::new(vec![Behavior::Fills, Behavior::Stalls]);
        let mut trade = AtomicTrade::new(vec![order(0, 100, 45), order(1, 100, 50)]);
        trade.timeout_ms = 25;
        match trade.execute(&client).await.unwrap_err() {
            // Not "no fills": leg 0 may well have filled at the venue.
            ExecutionError::Timeout { elapsed_ms } => assert_eq!(elapsed_ms, 25),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_trade_with_no_legs_does_nothing_rather_than_erroring() {
        let client = MockClient::new(Vec::new());
        let result = AtomicTrade::new(Vec::new()).execute(&client).await.unwrap();
        assert!(result.fills.is_empty());
        assert_eq!(result.total_cost_cents(), 0);
    }
}
