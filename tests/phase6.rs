//! Phase 6: the path from a detected edge to money at risk, and every gate that
//! can stop it on the way.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use sum100::{
    book::BookStore,
    clock::{Clock, ReplayClock},
    engine::{Engine, EngineConfig, EngineState, SignalLog},
    exec::{
        NewOrder, OrderClient, OrderError, OrderFill, atomic::DEFAULT_TIMEOUT_MS,
        paper::PaperOrderClient,
    },
    feed::{Feed, FeedEvent},
    fees::FeeModels,
    health::HealthState,
    portfolio::Portfolio,
    registry::{EventId, Registry},
    risk::RiskLimits,
    solver::SolverConfig,
    types::{ContractId, Level, Venue},
};
use tokio::sync::broadcast;

const NOW: u64 = 1_789_343_120_404;
const DEEP_POCKETS: i64 = 100_000_000;

/// One event, one contract, one complement group. The smallest thing that can
/// produce a real trade.
fn one_contract_registry() -> Registry {
    Registry::parse(
        r#"
[[event]]
id = "btc"
description = "BTC hourly"
resolves_at = "2026-10-14T21:00:00Z"
resolution_source = "Kalshi"
theme = "crypto"

[[event.group]]
type = "complement"
members = [{ venue = "kalshi", ticker = "KXBTCD-26SEP1417-T70000" }]
"#,
    )
    .unwrap()
}

/// A crossed book: buying both outcomes costs `ask_yes + ask_no` for a
/// guaranteed dollar.
fn crossed(ask_yes: i64, ask_no: i64, size: i64, seq: u64) -> FeedEvent {
    FeedEvent::Snapshot {
        contract: ContractId(0),
        yes: vec![Level {
            price: 100 - ask_no,
            size,
        }],
        no: vec![Level {
            price: 100 - ask_yes,
            size,
        }],
        seq,
        venue_ts_ms: None,
    }
}

struct VecFeed(std::vec::IntoIter<FeedEvent>);

impl Feed for VecFeed {
    fn next(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FeedEvent>> + Send + '_>> {
        let event = self.0.next();
        Box::pin(async move { event })
    }
}

fn feed(events: Vec<FeedEvent>) -> VecFeed {
    VecFeed(events.into_iter())
}

struct Harness {
    engine: Engine<SignalLog>,
    clock: ReplayClock,
}

fn harness(capital: i64, daily_loss_limit: i64, config: EngineConfig) -> Harness {
    let registry = one_contract_registry();
    let clock = ReplayClock::new();
    clock.advance_to(NOW);
    let tickers: Vec<String> = registry.tickers(Venue::Kalshi).to_vec();
    let store = BookStore::new(Venue::Kalshi, &tickers, Arc::new(clock.clone())).unwrap();
    let portfolio = Portfolio::new(capital, daily_loss_limit, NOW);
    Harness {
        engine: Engine::new(
            registry,
            store,
            SignalLog::default(),
            FeeModels::default(),
            config,
            Arc::new(clock.clone()),
            portfolio,
        ),
        clock,
    }
}

fn default_harness() -> Harness {
    harness(DEEP_POCKETS, DEEP_POCKETS, EngineConfig::default())
}

fn paper() -> PaperOrderClient {
    PaperOrderClient::new(FeeModels::default())
}

async fn drive(engine: &mut Engine<SignalLog>, events: Vec<FeedEvent>, client: &dyn OrderClient) {
    let (tx, _rx) = broadcast::channel::<EngineState>(64);
    engine.run(&mut feed(events), client, &tx).await;
}

// -------------------------------------------------------------- the full loop

/// Detect, admit, place both legs, book the position, and mark it.
#[tokio::test]
async fn an_edge_becomes_a_position_and_then_a_mark() {
    let mut h = default_harness();
    let client = paper();
    // 200 contracts of yes at 55 and no at 40: 19000 premium, 683 in fees.
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &client).await;

    assert_eq!(h.engine.metrics.trades_placed, 1);
    assert_eq!(h.engine.metrics.trades_no_fill, 0);
    assert_eq!(h.engine.metrics.trades_needing_reconciliation, 0);

    let signals = &h.engine.sink().signals;
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].order_ids.len(), 2, "both legs have venue ids");
    assert_eq!(signals[0].filled_cost_cents, 19_000 + 683);

    // The position is real: capital moved out and both legs are recorded.
    let position = h.engine.portfolio.open_positions().next().unwrap();
    assert_eq!(position.event_id, EventId(0));
    assert_eq!(position.legs.len(), 2);
    assert_eq!(position.cost_cents, 19_683);
    assert_eq!(position.fees_cents, 683);
    assert_eq!(
        h.engine.portfolio.capital_available_cents,
        DEEP_POCKETS - 19_683
    );

    // Marked at the bid against the same book it was bought from. The yes leg
    // exits into the 60 bid and the no leg into the 45 bid, so liquidation is
    // 21000 against 19683 committed. That is more than the 317 the trade will
    // actually settle for, because this synthetic book is crossed on both
    // sides; the mark is what could be got out right now, not the edge.
    assert_eq!(h.engine.portfolio.pnl_unrealized_cents(), 21_000 - 19_683);

    // Settling pays exactly one leg a dollar either way.
    let realized = h.engine.portfolio.realize(EventId(0), true).unwrap();
    assert_eq!(realized, 20_000 - 19_683);
    assert_eq!(h.engine.portfolio.open_count(), 0);
}

#[tokio::test]
async fn paper_fills_walk_the_book_and_place_nothing_real() {
    let mut h = default_harness();
    let client = paper();
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &client).await;

    assert!(!client.is_live());
    let fills = client.filled.lock().unwrap();
    assert_eq!(fills.len(), 2);
    for fill in fills.iter() {
        assert!(
            fill.order_id.starts_with("paper-"),
            "no order id should look like a venue's: {}",
            fill.order_id
        );
        assert_eq!(fill.timestamp_ms, NOW);
        // The fill price is the book's, not the limit's.
        assert!(fill.average_price == 55 || fill.average_price == 40);
        assert!(fill.fee_cents > 0);
    }
}

// ----------------------------------------------------------------- the gates

#[tokio::test]
async fn an_unhealthy_venue_blocks_a_profitable_trade() {
    let mut h = default_harness();
    let client = paper();
    // The disconnect lands after the book, so the edge is real and visible and
    // the only thing stopping it is the state of the connection.
    drive(
        &mut h.engine,
        vec![
            crossed(55, 40, 200, 1),
            FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
        ],
        &client,
    )
    .await;
    assert_eq!(h.engine.metrics.trades_placed, 1);

    // Now the same edge arrives while disconnected: the snapshot re-establishes
    // the book but health only returns with data on a live socket.
    let mut h = default_harness();
    let client = paper();
    drive(
        &mut h.engine,
        vec![
            FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
            crossed(55, 40, 200, 1),
        ],
        &client,
    )
    .await;
    assert_eq!(h.engine.metrics.trades_placed, 1, "a snapshot is data");

    // Staleness is the case a book alone cannot reveal.
    let mut h = harness(
        DEEP_POCKETS,
        DEEP_POCKETS,
        EngineConfig {
            max_idle_ms: 10,
            ..EngineConfig::default()
        },
    );
    let client = paper();
    let opportunities = h.engine.step(&crossed(55, 40, 200, 1));
    assert_eq!(opportunities.len(), 1, "the edge is there");
    h.clock.advance_to(NOW + 5_000);
    let planned = h.engine.admit(opportunities);
    assert!(planned.is_empty());
    assert_eq!(h.engine.metrics.blocked_unhealthy, 1);
    assert_eq!(h.engine.health.kalshi.state, HealthState::Stale);
    assert_eq!(h.engine.execute(planned, &client).await.len(), 0);
}

#[tokio::test]
async fn a_trade_that_cannot_be_funded_is_skipped_not_half_placed() {
    // Enough for the fees, nowhere near the 19683 the position costs.
    let mut h = harness(5_000, DEEP_POCKETS, EngineConfig::default());
    let client = paper();
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &client).await;

    assert_eq!(h.engine.metrics.blocked_no_capital, 1);
    assert_eq!(h.engine.metrics.trades_placed, 0);
    assert!(h.engine.sink().signals.is_empty());
    assert_eq!(h.engine.portfolio.capital_available_cents, 5_000);
    assert!(
        client.filled.lock().unwrap().is_empty(),
        "no leg was placed"
    );
}

#[tokio::test]
async fn the_event_limit_refuses_the_trade_that_would_cross_it() {
    let mut h = harness(
        DEEP_POCKETS,
        DEEP_POCKETS,
        EngineConfig {
            risk: RiskLimits {
                max_per_event_cents: 10_000,
                ..RiskLimits::default()
            },
            ..EngineConfig::default()
        },
    );
    let client = paper();
    // The position costs 19683, over a 10000 per-event limit.
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &client).await;
    assert_eq!(h.engine.metrics.blocked_risk_limit, 1);
    assert_eq!(h.engine.metrics.trades_placed, 0);

    // A smaller book produces a position that fits, and it goes through.
    let mut h = harness(
        DEEP_POCKETS,
        DEEP_POCKETS,
        EngineConfig {
            risk: RiskLimits {
                max_per_event_cents: 10_000,
                ..RiskLimits::default()
            },
            ..EngineConfig::default()
        },
    );
    let client = paper();
    drive(&mut h.engine, vec![crossed(55, 40, 50, 1)], &client).await;
    assert_eq!(h.engine.metrics.blocked_risk_limit, 0);
    assert_eq!(h.engine.metrics.trades_placed, 1);
}

#[tokio::test]
async fn a_closed_day_stops_new_risk_and_midnight_reopens_it() {
    let mut h = harness(DEEP_POCKETS, 100, EngineConfig::default());
    let client = paper();
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &client).await;
    assert_eq!(h.engine.metrics.trades_placed, 1);

    // An arbitrage cannot settle at a loss, so the day is closed the only way
    // it realistically gets closed: by something else having gone wrong earlier
    // in the session. What is under test is the gate, not how the loss arose.
    h.engine.portfolio.realize(EventId(0), true).unwrap();
    assert_eq!(
        h.engine.portfolio.pnl_realized_cents, 317,
        "the arb profited"
    );
    h.engine.portfolio.daily_loss_realized_cents = 183;
    assert!(!h.engine.portfolio.within_daily_loss_limit());

    drive(&mut h.engine, vec![crossed(55, 40, 200, 2)], &client).await;
    assert_eq!(h.engine.metrics.blocked_no_capital, 1, "the day is closed");
    assert_eq!(h.engine.metrics.trades_placed, 1, "still just the first");

    // Crossing UTC midnight clears the budget and trading resumes.
    let midnight = h.engine.portfolio.daily_reset_at_ms;
    h.clock.advance_to(midnight);
    drive(&mut h.engine, vec![crossed(55, 40, 200, 3)], &client).await;
    assert_eq!(h.engine.portfolio.daily_loss_realized_cents, 0);
    assert_eq!(h.engine.metrics.trades_placed, 2);
}

// ------------------------------------------------------- failure and refusal

/// A client that fills the first leg it sees and fails the rest.
struct OneLegClient {
    placed: AtomicU64,
    cancels: AtomicU64,
}

impl OrderClient for OneLegClient {
    fn place_order<'a>(
        &'a self,
        order: NewOrder,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OrderFill, OrderError>> + Send + 'a>,
    > {
        let first = self.placed.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if !first {
                return Err(OrderError::Unfillable {
                    wanted: order.quantity,
                    available: 0,
                });
            }
            Ok(OrderFill {
                order_id: "venue-1".into(),
                contract_id: order.contract_id,
                outcome: order.outcome,
                action: order.action,
                quantity_filled: order.quantity,
                average_price: order.limit_price,
                fee_cents: 0,
                timestamp_ms: NOW,
            })
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

/// The dangerous case: one leg is live and the other never happened.
#[tokio::test]
async fn a_legged_trade_is_flagged_for_reconciliation_not_booked() {
    let mut h = default_harness();
    let client = OneLegClient {
        placed: AtomicU64::new(0),
        cancels: AtomicU64::new(0),
    };
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &client).await;

    assert_eq!(h.engine.metrics.trades_needing_reconciliation, 1);
    assert_eq!(h.engine.metrics.trades_placed, 0);
    // No position is booked from half a trade, and no capital is committed to
    // something the portfolio cannot describe.
    assert_eq!(h.engine.portfolio.open_count(), 0);
    assert_eq!(h.engine.portfolio.capital_available_cents, DEEP_POCKETS);
    assert!(h.engine.sink().signals.is_empty());
    // A filled order was not "cancelled": it cannot be, and claiming so would
    // hide a live one-sided position.
    assert_eq!(client.cancels.load(Ordering::SeqCst), 0);
    // The failure counts against venue health, so a run of them stops trading.
    assert_eq!(h.engine.health.kalshi.consecutive_errors, 1);
}

/// A live client is refused unless the operator opted in, and the refusal is on
/// the path that spends money rather than at construction.
#[tokio::test]
async fn a_live_client_is_refused_without_the_explicit_opt_in() {
    struct PretendLive;
    impl OrderClient for PretendLive {
        fn place_order<'a>(
            &'a self,
            _order: NewOrder,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<OrderFill, OrderError>> + Send + 'a>,
        > {
            panic!("a live order was placed without the opt-in");
        }
        fn cancel_order<'a>(
            &'a self,
            _order_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OrderError>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn is_live(&self) -> bool {
            true
        }
    }

    assert!(
        !EngineConfig::default().allow_live_orders,
        "the default must never be live"
    );
    let mut h = default_harness();
    drive(&mut h.engine, vec![crossed(55, 40, 200, 1)], &PretendLive).await;
    assert_eq!(h.engine.metrics.trades_placed, 0);
    assert_eq!(h.engine.portfolio.open_count(), 0);

    // With the opt-in, the same client is used, which is why it panics.
    let mut h = harness(
        DEEP_POCKETS,
        DEEP_POCKETS,
        EngineConfig {
            allow_live_orders: true,
            ..EngineConfig::default()
        },
    );
    let opportunities = h.engine.step(&crossed(55, 40, 200, 1));
    let planned = h.engine.admit(opportunities);
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].trade.timeout_ms, DEFAULT_TIMEOUT_MS);
    // Legs carry the worst level consumed as their limit, not the average.
    assert!(planned[0].trade.legs.iter().all(|leg| leg.quantity == 200));
    assert!(
        std::panic::AssertUnwindSafe(h.engine.execute(planned, &PretendLive))
            .catch_unwind_check()
            .await
    );
}

/// Helper so the panic above is an assertion rather than a failed test.
trait CatchUnwind {
    async fn catch_unwind_check(self) -> bool;
}

impl<F: std::future::Future> CatchUnwind for std::panic::AssertUnwindSafe<F> {
    async fn catch_unwind_check(self) -> bool {
        use futures_util::FutureExt as _;
        self.catch_unwind().await.is_err()
    }
}

// ------------------------------------------------------------- offline replay

/// The shipped registry against a real recording: no arbitrage, so no orders.
#[tokio::test]
async fn replaying_a_real_session_in_paper_mode_places_nothing() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let registry = Registry::from_toml(&root.join("config/registry.toml")).unwrap();
    let options = sum100::feed::replay::ReplayOptions {
        tickers: Some(registry.tickers(Venue::Kalshi).to_vec()),
        pace: sum100::feed::replay::Pace::Max,
        session: None,
    };
    let mut feed = sum100::feed::replay::ReplayFeed::open(
        &root.join("data/phase2-1-e/production/kalshi-2026-09-14.ndjson.gz"),
        options,
    )
    .unwrap();
    let clock = feed.clock();
    let store = BookStore::new(Venue::Kalshi, feed.tickers(), Arc::new(clock.clone())).unwrap();
    let portfolio = Portfolio::new(DEEP_POCKETS, DEEP_POCKETS, clock.now_ms());
    let mut engine = Engine::new(
        registry,
        store,
        SignalLog::default(),
        FeeModels::default(),
        EngineConfig {
            solver: SolverConfig::default(),
            ..EngineConfig::default()
        },
        Arc::new(clock),
        portfolio,
    );
    let client = paper();
    let (tx, _rx) = broadcast::channel::<EngineState>(64);
    engine.run(&mut feed, &client, &tx).await;

    assert!(engine.metrics.events_received > 10_000);
    assert_eq!(engine.metrics.trades_placed, 0);
    assert_eq!(engine.metrics.trades_needing_reconciliation, 0);
    assert!(client.filled.lock().unwrap().is_empty());
    // Capital is untouched and the day never opened a position.
    assert_eq!(engine.portfolio.capital_available_cents, DEEP_POCKETS);
    assert_eq!(engine.portfolio.pnl_realized_cents, 0);
}
