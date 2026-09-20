//! Phase 5 tests: registry loading, dirty marking, the engine loop, and the
//! determinism that makes a replay worth running.

use std::{path::Path, sync::Arc};
use sum100::{
    book::BookStore,
    clock::ReplayClock,
    config::Config,
    engine::{Engine, EngineState, SignalLog},
    feed::{
        Feed, FeedEvent,
        kalshi::Parser,
        replay::{Pace, ReplayFeed, ReplayOptions},
    },
    fees::FeeModels,
    registry::{GroupId, Registry},
    solver::{Solver, SolverConfig},
    types::{Cents, ContractId, Level, Side, Venue},
};
use tokio::sync::broadcast;

const NOW: u64 = 1_789_343_120_404;

/// The four tickers recorded in the phase 3 session A capture.
const PHASE3A: [&str; 4] = [
    "KXBTCD-26SEP1417-T76999.99",
    "KXBTCD-26SEP1417-T77499.99",
    "KXBTCD-26SEP1417-T77749.99",
    "KXBTCD-26SEP1417-T77999.99",
];

fn phase3a_file() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data/phase3-a/production/kalshi-2026-09-14.ndjson.gz")
}

/// A registry over the phase 3 session A tickers: one complement per strike,
/// plus the ladder they form.
fn phase3a_registry() -> Registry {
    let mut text = String::from(
        "[[event]]\nid = \"btc\"\ndescription = \"BTC hourly\"\nresolves_at = \"2026-09-14T21:00:00Z\"\nresolution_source = \"Kalshi\"\n",
    );
    for ticker in PHASE3A {
        text.push_str(&format!(
            "\n[[event.group]]\ntype = \"complement\"\nmembers = [{{ venue = \"kalshi\", ticker = \"{ticker}\" }}]\n"
        ));
    }
    text.push_str("\n[[event.group]]\ntype = \"monotone\"\nmembers = [\n");
    for ticker in PHASE3A {
        text.push_str(&format!(
            "  {{ venue = \"kalshi\", ticker = \"{ticker}\" }},\n"
        ));
    }
    text.push_str("]\n");
    Registry::parse(&text).unwrap()
}

/// A feed that yields a fixed script, for driving the loop without a socket.
struct VecFeed {
    events: std::vec::IntoIter<FeedEvent>,
    pub resyncs: Arc<std::sync::atomic::AtomicU64>,
}

impl VecFeed {
    fn new(events: Vec<FeedEvent>) -> Self {
        VecFeed {
            events: events.into_iter(),
            resyncs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl Feed for VecFeed {
    fn next(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<FeedEvent>> + Send + '_>> {
        let event = self.events.next();
        Box::pin(async move { event })
    }

    fn request_resync(&self) {
        self.resyncs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn engine_over(registry: Registry, clock: ReplayClock) -> Engine<SignalLog> {
    let tickers: Vec<String> = registry.tickers(Venue::Kalshi).to_vec();
    let store = BookStore::new(Venue::Kalshi, &tickers, Arc::new(clock.clone())).unwrap();
    Engine::new(
        registry,
        store,
        Solver::new(),
        SignalLog::default(),
        FeeModels::default(),
        SolverConfig::default(),
        Arc::new(clock),
    )
}

/// Snapshot making one contract quote `ask_yes` and `ask_no`.
fn quote(contract: u32, ask_yes: Cents, ask_no: Cents, size: i64, seq: u64) -> FeedEvent {
    FeedEvent::Snapshot {
        contract: ContractId(contract),
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

// ------------------------------------------------------------ identity wiring

/// Contract ids are positional, so the registry, the parser, and the book store
/// must be built from one ordered list or every id silently shifts.
#[test]
fn registry_parser_and_book_store_agree_on_contract_ids() {
    let registry = phase3a_registry();
    let tickers: Vec<String> = registry.tickers(Venue::Kalshi).to_vec();
    assert_eq!(tickers, PHASE3A);

    let parser = Parser::new(&tickers).unwrap();
    let store = BookStore::new(Venue::Kalshi, &tickers, Arc::new(ReplayClock::new())).unwrap();
    for (index, ticker) in tickers.iter().enumerate() {
        let expected = ContractId(index as u32);
        assert_eq!(
            registry.contracts().get(Venue::Kalshi, ticker),
            Some(expected)
        );
        assert_eq!(parser.contracts.get(Venue::Kalshi, ticker), Some(expected));
        assert_eq!(store.contracts().get(Venue::Kalshi, ticker), Some(expected));
        assert_eq!(registry.binding(expected).unwrap().venue_ticker, *ticker);
    }
}

/// The shipped registry and config load together and describe the phase 3
/// session D capture, which is what `scan --replay` is pointed at.
#[test]
fn shipped_config_and_registry_load_together() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = Config::load(&root.join("config/example.toml")).unwrap();
    let registry = Registry::from_toml(&root.join(config.registry.path.clone())).unwrap();
    assert_eq!(registry.tickers(Venue::Kalshi).len(), 80);
    assert_eq!(config.engine.max_book_age_ms, 500);
    // Every contract is in exactly its own complement group and the ladder.
    let ladder = registry.groups().last().unwrap();
    assert_eq!(ladder.members().len(), 80);
}

// -------------------------------------------------------------- dirty marking

#[test]
fn an_update_dirties_exactly_the_groups_containing_that_contract() {
    let registry = phase3a_registry();
    let tickers: Vec<String> = registry.tickers(Venue::Kalshi).to_vec();
    let clock = ReplayClock::new();
    clock.advance_to(NOW);
    let mut store = BookStore::new(Venue::Kalshi, &tickers, Arc::new(clock)).unwrap();

    let (applied, dirty) = store.apply_and_mark(&quote(2, 50, 50, 10, 1));
    assert!(matches!(applied, sum100::book::Applied::Snapshot(_)));
    assert_eq!(dirty, vec![ContractId(2)]);
    // Contract 2 is in its own complement group and in the ladder, and nothing
    // else. The ladder is group 4, after the four complements.
    assert_eq!(
        registry.groups_for(ContractId(2)),
        &[GroupId(2), GroupId(4)]
    );
    assert_eq!(
        registry.groups_for(ContractId(0)),
        &[GroupId(0), GroupId(4)]
    );

    // A delta out of sequence marks nothing: every book is held for resync, and
    // the solver would reject each group as not live.
    let gap = FeedEvent::Delta {
        contract: ContractId(2),
        side: Side::Yes,
        price: 40,
        size_delta: 5,
        seq: 99,
        venue_ts_ms: NOW,
    };
    let (applied, dirty) = store.apply_and_mark(&gap);
    assert!(matches!(applied, sum100::book::Applied::Gap { .. }));
    assert!(dirty.is_empty());

    // So does a disconnect.
    let (_, dirty) = store.apply_and_mark(&FeedEvent::Disconnected {
        venue: Venue::Kalshi,
    });
    assert!(dirty.is_empty());
}

// ----------------------------------------------------------------- the loop

#[tokio::test]
async fn the_loop_solves_dirty_groups_and_hands_signals_to_the_sink() {
    let clock = ReplayClock::new();
    clock.advance_to(NOW);
    let mut engine = engine_over(phase3a_registry(), clock);

    // Three coherent strikes, then one crossed book: yes at 55 and no at 40
    // costs 95 cents for a guaranteed dollar.
    let mut feed = VecFeed::new(vec![
        quote(0, 60, 41, 200, 1),
        quote(1, 50, 51, 200, 2),
        quote(2, 40, 61, 200, 3),
        quote(3, 55, 40, 200, 4),
    ]);
    let (tx, mut rx) = broadcast::channel::<EngineState>(64);
    engine.run(&mut feed, &tx).await;
    drop(tx);

    // The last snapshot breaks two constraints at once: contract 3's own yes and
    // no now cost 95 together, and buying rung 2 at 40 while rung 3 bids 60
    // crosses the ladder. Both are real and they consume different liquidity.
    let signals = &engine.sink().signals;
    assert_eq!(signals.len(), 2, "expected both violations: {signals:?}");

    // Ranked by return, so the ladder's 20 cent gap leads the complement's 5.
    let ladder = &signals[0];
    assert_eq!(ladder.group, 4);
    assert_eq!(ladder.legs[0].contract, 2);
    assert_eq!(ladder.legs[0].side, "yes");
    assert_eq!(ladder.legs[1].contract, 3);
    assert_eq!(ladder.legs[1].side, "no");
    assert_eq!(ladder.capital_cents, 16_000 + 336 + 336);
    assert_eq!(ladder.net_cents, 20_000 - 16_000 - 672);

    let complement = &signals[1];
    assert_eq!(complement.group, 3, "contract 3's own complement group");
    assert_eq!(complement.legs.len(), 2);
    assert_eq!(complement.legs[0].side, "yes");
    assert_eq!(complement.legs[1].side, "no");
    assert_eq!(complement.qty, 200);
    // 200 contracts at 95 cents, fees 347 at 55c and 336 at 40c.
    assert_eq!(complement.capital_cents, 19_000 + 347 + 336);
    assert_eq!(complement.net_cents, 20_000 - 19_000 - 683);
    assert!(ladder.annualized_return_percent > complement.annualized_return_percent);
    assert_eq!(engine.metrics.signals_accepted, 2);

    // Every applied event published state, and the last one carries the signals.
    let mut states = Vec::new();
    while let Ok(state) = rx.try_recv() {
        states.push(state);
    }
    assert_eq!(states.len(), 4);
    assert_eq!(states[3].opportunities.len(), 2);
    assert!(states[0].opportunities.is_empty());
    assert_eq!(states[3].contracts.len(), 4);
    assert_eq!(states[3].contracts[3].best_ask, Some(55));
    // The ladder was unevaluable until its last member had a book.
    assert_eq!(engine.solver_metrics().rejected_not_live, 3);
}

#[tokio::test]
async fn each_dirty_group_is_evaluated_exactly_once_per_update() {
    let clock = ReplayClock::new();
    clock.advance_to(NOW);
    let mut engine = engine_over(phase3a_registry(), clock);
    let mut feed = VecFeed::new((0..4).map(|i| quote(i, 50, 51, 10, i as u64 + 1)).collect());
    let (tx, _rx) = broadcast::channel::<EngineState>(64);
    engine.run(&mut feed, &tx).await;

    // Four updates, each touching one complement group and the shared ladder:
    // two groups per update, never the ladder twice for one event.
    assert_eq!(engine.metrics.events_received, 4);
    assert_eq!(engine.metrics.books_updated, 4);
    assert_eq!(engine.metrics.groups_dirtied, 8);
    assert_eq!(engine.solver_metrics().groups_evaluated, 8);
}

#[tokio::test]
async fn a_sequence_gap_asks_the_feed_to_resync_and_stops_solving() {
    let clock = ReplayClock::new();
    clock.advance_to(NOW);
    let mut engine = engine_over(phase3a_registry(), clock);
    let mut feed = VecFeed::new(vec![
        quote(0, 60, 41, 10, 1),
        // Expected seq 2; this is 9.
        FeedEvent::Delta {
            contract: ContractId(0),
            side: Side::Yes,
            price: 40,
            size_delta: 5,
            seq: 9,
            venue_ts_ms: NOW,
        },
        quote(3, 55, 40, 10, 10),
    ]);
    let resyncs = feed.resyncs.clone();
    let (tx, _rx) = broadcast::channel::<EngineState>(64);
    engine.run(&mut feed, &tx).await;

    assert_eq!(engine.metrics.sequence_gaps, 1);
    assert_eq!(resyncs.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(engine.metrics.resyncs_requested, 1);
    // The post-gap snapshot is absolute, so its own book solves again — but the
    // crossed book it describes is not signalled, because contract 3's ladder
    // partners are still held for resync and its complement group is live.
    assert_eq!(engine.sink().signals.len(), 1);
    // Twice: once before contract 3 had a book, once after the gap held the rest.
    assert_eq!(engine.solver_metrics().rejected_not_live, 2);
}

// ------------------------------------------------------------- determinism

/// Drive the real phase 3 session A capture and return everything observable.
async fn replay_once(session: usize) -> (Vec<sum100::engine::Signal>, Vec<EngineState>) {
    let registry = phase3a_registry();
    let options = ReplayOptions {
        tickers: Some(registry.tickers(Venue::Kalshi).to_vec()),
        pace: Pace::Max,
        session: Some(session),
    };
    let mut feed = ReplayFeed::open(&phase3a_file(), options).unwrap();
    let clock = feed.clock();
    let store = BookStore::new(Venue::Kalshi, feed.tickers(), Arc::new(clock.clone())).unwrap();
    let mut engine = Engine::new(
        registry,
        store,
        Solver::new(),
        SignalLog::default(),
        FeeModels::default(),
        SolverConfig::default(),
        Arc::new(clock),
    );
    let (tx, mut rx) = broadcast::channel::<EngineState>(65_536);
    engine.run(&mut feed, &tx).await;
    feed.finish().unwrap();
    drop(tx);
    let mut states = Vec::new();
    while let Ok(state) = rx.try_recv() {
        states.push(state);
    }
    (engine.sink().signals.clone(), states)
}

/// The point of replay: the same file through the same engine is the same run.
#[tokio::test]
async fn replaying_a_recorded_session_twice_produces_identical_output() {
    let (signals_a, states_a) = replay_once(1).await;
    let (signals_b, states_b) = replay_once(1).await;

    assert!(!states_a.is_empty(), "the capture produced no engine state");
    assert_eq!(signals_a, signals_b);
    assert_eq!(states_a.len(), states_b.len());
    // Compare the serialized stream, which is exactly what the API layer would
    // have published, rather than only the fields this test happens to name.
    assert_eq!(
        serde_json::to_string(&states_a).unwrap(),
        serde_json::to_string(&states_b).unwrap()
    );
    // Nothing time-dependent leaked in: engine timestamps come from the replay
    // clock, so they repeat exactly too.
    assert_eq!(states_a[0].timestamp_ms, states_b[0].timestamp_ms);
    assert_eq!(
        states_a.last().unwrap().engine,
        states_b.last().unwrap().engine
    );
    assert_eq!(
        states_a.last().unwrap().solver,
        states_b.last().unwrap().solver
    );
}

/// A live Kalshi book is coherent, and the engine says so rather than inventing
/// a trade. This is the shape of nearly every real evaluation.
#[tokio::test]
async fn the_recorded_session_is_coherent_and_produces_no_signal() {
    let (signals, states) = replay_once(1).await;
    assert!(signals.is_empty(), "unexpected signals: {signals:?}");
    let last = states.last().unwrap();
    assert!(last.engine.books_updated > 100, "{:?}", last.engine);
    assert_eq!(last.solver.opportunities_emitted, 0);
    assert_eq!(last.solver.candidates_found, 0);
    // Publishing is skipped entirely when nobody subscribes, so a verification
    // run costs no snapshot allocation. The final state's own counter is one
    // behind the total, because a message cannot count itself.
    assert_eq!(last.engine.states_broadcast + 1, states.len() as u64);
}
