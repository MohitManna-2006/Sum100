use std::{fs, path::Path, sync::Arc};
use sum100::{
    book::{Applied, BookStore},
    clock::{Clock, ReplayClock},
    feed::{FeedEvent, kalshi::Parser},
    types::{BookState, ContractId, Level, Side, TokenSide, Venue},
};

fn fixture(name: &str) -> Vec<String> {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
    .lines()
    .map(str::to_owned)
    .collect()
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(ReplayClock::new())
}

fn ticker() -> String {
    "KXBTCD-26SEP1417-T76999.99".into()
}

fn assert_book_invariants(store: &BookStore) {
    for book in store.books() {
        for price in 0..=100 {
            let y = book.yes_size_at(price).unwrap();
            let n = book.no_size_at(price).unwrap();
            assert!(y >= 0, "negative yes at {price}");
            assert!(n >= 0, "negative no at {price}");
        }
        for level in book.bids().chain(book.asks()) {
            assert!((0..=100).contains(&level.price));
            assert!(level.size > 0);
        }
    }
}

#[test]
fn parser_and_bookstore_agree_on_contract_ids() {
    let tickers = vec![ticker()];
    let parser = Parser::new(&tickers).unwrap();
    let store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let a = parser.contracts.get(Venue::Kalshi, &ticker()).unwrap();
    let b = store.contracts().get(Venue::Kalshi, &ticker()).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, ContractId(0));
}

#[test]
fn full_fixture_replay_reaches_pinned_final_book() {
    let tickers = vec![ticker()];
    let mut parser = Parser::new(&tickers).unwrap();
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let mut gaps = 0u64;
    for raw in fixture("stage2-live-orderbook.ndjson") {
        if let Some(event) = parser.parse(&raw, 1789343120404) {
            let applied = store.apply(&event);
            if matches!(applied, Applied::Gap { .. }) {
                gaps += 1;
            }
            assert_book_invariants(&store);
        }
    }
    assert_eq!(parser.metrics.parse_errors, 0);
    assert_eq!(gaps, 0);
    assert_eq!(store.metrics.sequence_gaps, 0);
    assert_eq!(store.metrics.negative_level_clamps, 0);
    let book = store.get(ContractId(0)).unwrap();
    assert_eq!(book.state, BookState::Live);
    assert_eq!(book.seq, 461);
    assert_eq!(store.expected_seq(Venue::Kalshi), Some(462));
    assert_eq!(book.best_bid().map(|l| (l.price, l.size)), Some((44, 4294)));
    assert_eq!(book.best_ask().map(|l| (l.price, l.size)), Some((46, 100)));
    assert_eq!(
        book.best_no_bid().map(|l| (l.price, l.size)),
        Some((54, 100))
    );
    assert_eq!(book.bids().count(), 26);
    assert_eq!(book.asks().count(), 26);
    assert_eq!(store.metrics.snapshots_applied, 1);
    assert_eq!(store.metrics.deltas_applied, 460);
}

#[test]
fn no_side_delta_becomes_yes_ask_at_complement() {
    let tickers = vec![ticker()];
    let mut parser = Parser::new(&tickers).unwrap();
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    // Snapshot then deltas through seq 4 (the captured no-side frame).
    for raw in fixture("stage2-live-orderbook.ndjson").into_iter().take(5) {
        if let Some(event) = parser.parse(&raw, 0) {
            store.apply(&event);
        }
    }
    let book = store.get(ContractId(0)).unwrap();
    // Wire: no @ 55 with delta -1000 from snapshot 2105 -> 1105.
    assert_eq!(book.no_size_at(55), Some(1105));
    assert_eq!(
        book.asks().find(|l| l.price == 45).map(|l| l.size),
        Some(1105)
    );
    let no_frame = &fixture("no-side-delta.json")[0];
    assert_eq!(
        parser.parse(no_frame, 0),
        Some(FeedEvent::Delta {
            contract: ContractId(0),
            side: Side::No,
            price: 55,
            size_delta: -1000,
            seq: 4,
            venue_ts_ms: 1789343119239,
        })
    );
}

#[test]
fn sequence_gap_marks_resyncing_and_preserves_contents() {
    let tickers = vec![ticker()];
    let mut parser = Parser::new(&tickers).unwrap();
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let frames = fixture("stage2-live-orderbook.ndjson");
    // Snapshot + first delta (seq 2).
    for raw in frames.iter().take(3) {
        if let Some(event) = parser.parse(raw, 0) {
            assert!(!matches!(store.apply(&event), Applied::Gap { .. }));
        }
    }
    let before_yes = store.get(ContractId(0)).unwrap().yes_size_at(41);
    let before_no = store.get(ContractId(0)).unwrap().no_size_at(55);
    // Skip seq 3; apply seq 4.
    let gap_raw = &frames[4];
    let event = parser.parse(gap_raw, 0).expect("delta");
    assert_eq!(
        store.apply(&event),
        Applied::Gap {
            expected: 3,
            got: 4
        }
    );
    let book = store.get(ContractId(0)).unwrap();
    assert_eq!(book.state, BookState::Resyncing);
    assert_eq!(book.yes_size_at(41), before_yes);
    assert_eq!(book.no_size_at(55), before_no);
    assert_eq!(store.metrics.sequence_gaps, 1);
    assert_eq!(store.expected_seq(Venue::Kalshi), None);
}

#[test]
fn deltas_dropped_while_resyncing_until_snapshot() {
    let tickers = vec![ticker()];
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let id = ContractId(0);
    assert_eq!(
        store.apply(&FeedEvent::Snapshot {
            contract: id,
            yes: vec![Level {
                price: 40,
                size: 10
            }],
            no: vec![Level {
                price: 50,
                size: 20
            }],
            seq: 1,
            venue_ts_ms: None,
        }),
        Applied::Snapshot(id)
    );
    assert_eq!(
        store.apply(&FeedEvent::Delta {
            contract: id,
            side: Side::Yes,
            price: 40,
            size_delta: 5,
            seq: 2,
            venue_ts_ms: 1001,
        }),
        Applied::Delta(id)
    );
    assert_eq!(
        store.apply(&FeedEvent::Delta {
            contract: id,
            side: Side::Yes,
            price: 40,
            size_delta: 1,
            seq: 4,
            venue_ts_ms: 1002,
        }),
        Applied::Gap {
            expected: 3,
            got: 4
        }
    );
    assert_eq!(
        store.apply(&FeedEvent::Delta {
            contract: id,
            side: Side::Yes,
            price: 40,
            size_delta: 99,
            seq: 5,
            venue_ts_ms: 1003,
        }),
        Applied::Skipped
    );
    assert_eq!(store.get(id).unwrap().yes_size_at(40), Some(15));
    assert_eq!(store.metrics.deltas_skipped_not_live, 1);

    assert_eq!(
        store.apply(&FeedEvent::Snapshot {
            contract: id,
            yes: vec![Level { price: 41, size: 7 }],
            no: vec![],
            seq: 10,
            venue_ts_ms: None,
        }),
        Applied::Snapshot(id)
    );
    let book = store.get(id).unwrap();
    assert_eq!(book.state, BookState::Live);
    assert_eq!(book.seq, 10);
    assert_eq!(book.yes_size_at(41), Some(7));
    assert_eq!(book.yes_size_at(40), Some(0));
    assert_eq!(store.expected_seq(Venue::Kalshi), Some(11));
}

#[test]
fn disconnect_invalidates_and_resubscribe_awaits_snapshot() {
    let tickers = vec![ticker()];
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let id = ContractId(0);
    store.apply(&FeedEvent::Snapshot {
        contract: id,
        yes: vec![Level { price: 30, size: 5 }],
        no: vec![],
        seq: 1,
        venue_ts_ms: None,
    });
    assert_eq!(
        store.apply(&FeedEvent::Disconnected {
            venue: Venue::Kalshi
        }),
        Applied::Invalidated
    );
    assert_eq!(store.get(id).unwrap().state, BookState::Resyncing);
    assert_eq!(store.get(id).unwrap().yes_size_at(30), Some(5));

    assert_eq!(
        store.apply(&FeedEvent::Resubscribed { contract: id }),
        Applied::Invalidated
    );
    assert_eq!(
        store.apply(&FeedEvent::Delta {
            contract: id,
            side: Side::Yes,
            price: 30,
            size_delta: 1,
            seq: 1,
            venue_ts_ms: 2,
        }),
        Applied::Skipped
    );
    store.apply(&FeedEvent::Snapshot {
        contract: id,
        yes: vec![Level { price: 31, size: 9 }],
        no: vec![],
        seq: 1,
        venue_ts_ms: None,
    });
    assert_eq!(store.get(id).unwrap().state, BookState::Live);
    assert_eq!(store.get(id).unwrap().yes_size_at(31), Some(9));
}

#[test]
fn level_insert_update_and_remove() {
    let tickers = vec![ticker()];
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let id = ContractId(0);
    store.apply(&FeedEvent::Snapshot {
        contract: id,
        yes: vec![],
        no: vec![],
        seq: 1,
        venue_ts_ms: None,
    });
    // Insert 0 -> n
    store.apply(&FeedEvent::Delta {
        contract: id,
        side: Side::Yes,
        price: 25,
        size_delta: 100,
        seq: 2,
        venue_ts_ms: 2,
    });
    assert_eq!(store.get(id).unwrap().yes_size_at(25), Some(100));
    assert!(
        store
            .get(id)
            .unwrap()
            .bids()
            .any(|l| l.price == 25 && l.size == 100)
    );
    // Update n -> m
    store.apply(&FeedEvent::Delta {
        contract: id,
        side: Side::Yes,
        price: 25,
        size_delta: 50,
        seq: 3,
        venue_ts_ms: 3,
    });
    assert_eq!(store.get(id).unwrap().yes_size_at(25), Some(150));
    // Remove n -> 0
    store.apply(&FeedEvent::Delta {
        contract: id,
        side: Side::Yes,
        price: 25,
        size_delta: -150,
        seq: 4,
        venue_ts_ms: 4,
    });
    assert_eq!(store.get(id).unwrap().yes_size_at(25), Some(0));
    assert!(!store.get(id).unwrap().bids().any(|l| l.price == 25));
}

#[test]
fn floor_drift_clamps_at_zero() {
    let tickers = vec![ticker()];
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let id = ContractId(0);
    // Snapshot size 0 (as if "0.01" floored), then remove 1.
    store.apply(&FeedEvent::Snapshot {
        contract: id,
        yes: vec![Level { price: 10, size: 0 }],
        no: vec![],
        seq: 1,
        venue_ts_ms: None,
    });
    assert_eq!(
        store.apply(&FeedEvent::Delta {
            contract: id,
            side: Side::Yes,
            price: 10,
            size_delta: -1,
            seq: 2,
            venue_ts_ms: 2,
        }),
        Applied::Delta(id)
    );
    assert_eq!(store.get(id).unwrap().yes_size_at(10), Some(0));
    assert_eq!(store.get(id).unwrap().state, BookState::Live);
    assert_eq!(store.metrics.negative_level_clamps, 1);
}

#[test]
fn crossed_book_stays_live_and_is_counted() {
    let tickers = vec![ticker()];
    let mut store = BookStore::new(Venue::Kalshi, &tickers, clock()).unwrap();
    let id = ContractId(0);
    store.apply(&FeedEvent::Snapshot {
        contract: id,
        yes: vec![Level {
            price: 60,
            size: 10,
        }],
        no: vec![Level {
            price: 50,
            size: 10,
        }],
        seq: 1,
        venue_ts_ms: None,
    });
    let book = store.get(id).unwrap();
    assert!(book.is_crossed());
    assert_eq!(book.state, BookState::Live);
    assert_eq!(book.best_bid().map(|l| l.price), Some(60));
    assert_eq!(book.best_ask().map(|l| l.price), Some(50));
    assert!(store.metrics.crossed_books_observed >= 1);
}

/// Two venues, two subscriptions, two sequence spaces.
///
/// Kalshi and Polymarket number their own streams from their own handshakes, so
/// a store holding both must not compare one against the other. Before this was
/// scoped per venue, every alternating message read as a discontinuity.
fn dual_venue_store() -> BookStore {
    let kalshi = vec![ticker()];
    let polymarket = vec!["0xabc".to_owned()];
    BookStore::multi_venue(
        &[(Venue::Kalshi, &kalshi), (Venue::Polymarket, &polymarket)],
        clock(),
    )
    .unwrap()
}

fn snapshot(contract: ContractId, seq: u64) -> FeedEvent {
    FeedEvent::Snapshot {
        contract,
        yes: vec![Level {
            price: 40,
            size: 10,
        }],
        no: vec![Level {
            price: 50,
            size: 20,
        }],
        seq,
        venue_ts_ms: None,
    }
}

fn delta(contract: ContractId, seq: u64) -> FeedEvent {
    FeedEvent::Delta {
        contract,
        side: Side::Yes,
        price: 40,
        size_delta: 5,
        seq,
        venue_ts_ms: 1_000,
    }
}

#[test]
fn interleaved_venues_do_not_read_as_sequence_gaps() {
    let mut store = dual_venue_store();
    let (kalshi, polymarket) = (ContractId(0), ContractId(1));

    // Each venue starts its own sequence at 1, and they interleave.
    assert_eq!(store.apply(&snapshot(kalshi, 1)), Applied::Snapshot(kalshi));
    assert_eq!(
        store.apply(&snapshot(polymarket, 1)),
        Applied::Snapshot(polymarket)
    );
    assert_eq!(store.apply(&delta(kalshi, 2)), Applied::Delta(kalshi));
    assert_eq!(
        store.apply(&delta(polymarket, 2)),
        Applied::Delta(polymarket)
    );
    assert_eq!(store.apply(&delta(kalshi, 3)), Applied::Delta(kalshi));

    assert_eq!(store.metrics.sequence_gaps, 0);
    assert_eq!(store.expected_seq(Venue::Kalshi), Some(4));
    assert_eq!(store.expected_seq(Venue::Polymarket), Some(3));
}

#[test]
fn a_gap_on_one_venue_leaves_the_other_live() {
    let mut store = dual_venue_store();
    let (kalshi, polymarket) = (ContractId(0), ContractId(1));
    store.apply(&snapshot(kalshi, 1));
    store.apply(&snapshot(polymarket, 1));

    // Skip Kalshi seq 2.
    assert_eq!(
        store.apply(&delta(kalshi, 3)),
        Applied::Gap {
            expected: 2,
            got: 3
        }
    );

    assert_eq!(store.get(kalshi).unwrap().state, BookState::Resyncing);
    assert_eq!(store.get(polymarket).unwrap().state, BookState::Live);
    assert_eq!(store.expected_seq(Venue::Kalshi), None);
    assert_eq!(store.expected_seq(Venue::Polymarket), Some(2));

    // The untouched venue keeps applying its own deltas.
    assert_eq!(
        store.apply(&delta(polymarket, 2)),
        Applied::Delta(polymarket)
    );
    assert_eq!(store.metrics.sequence_gaps, 1);
}

#[test]
fn a_disconnect_invalidates_only_its_own_venue() {
    let mut store = dual_venue_store();
    let (kalshi, polymarket) = (ContractId(0), ContractId(1));
    store.apply(&snapshot(kalshi, 1));
    store.apply(&snapshot(polymarket, 1));

    assert_eq!(
        store.apply(&FeedEvent::Disconnected {
            venue: Venue::Polymarket
        }),
        Applied::Invalidated
    );
    assert_eq!(store.get(kalshi).unwrap().state, BookState::Live);
    assert_eq!(store.get(polymarket).unwrap().state, BookState::Resyncing);
    assert_eq!(store.expected_seq(Venue::Kalshi), Some(2));
    assert_eq!(store.expected_seq(Venue::Polymarket), None);

    // A resubscribe names a contract, and only that contract's venue resets.
    store.apply(&delta(kalshi, 2));
    assert_eq!(
        store.apply(&FeedEvent::Resubscribed {
            contract: polymarket
        }),
        Applied::Invalidated
    );
    assert_eq!(store.get(kalshi).unwrap().state, BookState::Live);
    assert_eq!(store.expected_seq(Venue::Kalshi), Some(3));
}

#[test]
fn an_event_routes_to_the_venue_that_owns_its_contract() {
    let store = dual_venue_store();
    let (kalshi, polymarket) = (ContractId(0), ContractId(1));
    assert_eq!(store.venue_of(&delta(kalshi, 1)), Some(Venue::Kalshi));
    assert_eq!(
        store.venue_of(&snapshot(polymarket, 1)),
        Some(Venue::Polymarket)
    );
    assert_eq!(
        store.venue_of(&FeedEvent::Resubscribed {
            contract: polymarket
        }),
        Some(Venue::Polymarket)
    );
    // A contract this store does not hold belongs to no venue here.
    assert_eq!(store.venue_of(&delta(ContractId(99), 1)), None);
    assert_eq!(store.venues(), vec![Venue::Kalshi, Venue::Polymarket]);
}

/// A venue that restates a level rather than adjusting it.
///
/// Applying an absolute size through the delta path would add it to what is
/// already resting and double the level on its first update, which is why this
/// is a separate operation rather than a flag on the other one.
fn polymarket_store() -> BookStore {
    let tokens = vec!["0xtoken".to_owned()];
    BookStore::multi_venue(&[(Venue::Polymarket, &tokens)], clock()).unwrap()
}

#[test]
fn a_level_set_replaces_the_resting_size_rather_than_adding_to_it() {
    let mut store = polymarket_store();
    let id = ContractId(0);
    assert_eq!(
        store.apply(&snapshot_with(id, 1, &[(57, 1_000)], &[])),
        Applied::Snapshot(id)
    );
    assert_eq!(store.get(id).unwrap().yes_size_at(57), Some(1_000));

    // Absolute: the level becomes 2_284_194, not 1_000 + 2_284_194.
    assert_eq!(
        store.apply(&FeedEvent::LevelSet {
            contract: id,
            side: TokenSide::Bid,
            price: 57,
            size: 2_284_194,
            venue_ts_ms: 1_700,
        }),
        Applied::LevelSet(id)
    );
    assert_eq!(store.get(id).unwrap().yes_size_at(57), Some(2_284_194));
    assert_eq!(store.metrics.levels_replaced, 1);

    // And a level can be emptied, which a signed delta would have to guess at.
    store.apply(&FeedEvent::LevelSet {
        contract: id,
        side: TokenSide::Bid,
        price: 57,
        size: 0,
        venue_ts_ms: 1_800,
    });
    assert_eq!(store.get(id).unwrap().yes_size_at(57), Some(0));
}

#[test]
fn an_ask_level_set_is_stored_as_a_bid_on_the_complement() {
    let mut store = polymarket_store();
    let id = ContractId(0);
    store.apply(&snapshot_with(id, 1, &[(55, 10)], &[]));

    // Offering this outcome at 57 is bidding 43 for the other one.
    store.apply(&FeedEvent::LevelSet {
        contract: id,
        side: TokenSide::Ask,
        price: 57,
        size: 900,
        venue_ts_ms: 1_700,
    });
    let book = store.get(id).unwrap();
    assert_eq!(book.no_size_at(43), Some(900));
    assert_eq!(book.yes_size_at(57), Some(0), "the yes ladder is untouched");
    // Read back through the derived ask ladder, it is the price the venue sent.
    assert_eq!(book.best_ask().map(|l| l.price), Some(57));
}

#[test]
fn a_level_set_is_refused_until_a_snapshot_has_landed() {
    let mut store = polymarket_store();
    let id = ContractId(0);
    // No snapshot yet: an absolute level would describe a book with one price
    // in it rather than the real one.
    assert_eq!(
        store.apply(&FeedEvent::LevelSet {
            contract: id,
            side: TokenSide::Bid,
            price: 57,
            size: 100,
            venue_ts_ms: 1_700,
        }),
        Applied::Skipped
    );
    assert_eq!(store.metrics.levels_replaced, 0);
    assert_eq!(store.metrics.deltas_skipped_not_live, 1);
}

#[test]
fn a_level_set_out_of_range_or_negative_is_skipped_not_applied() {
    let mut store = polymarket_store();
    let id = ContractId(0);
    store.apply(&snapshot_with(id, 1, &[(55, 10)], &[]));

    for (side, price, size) in [
        (TokenSide::Bid, 101, 5),
        (TokenSide::Bid, -1, 5),
        (TokenSide::Bid, 55, -5),
    ] {
        assert_eq!(
            store.apply(&FeedEvent::LevelSet {
                contract: id,
                side,
                price,
                size,
                venue_ts_ms: 1_700,
            }),
            Applied::Skipped,
            "price {price} size {size}"
        );
    }
    assert_eq!(store.metrics.levels_replaced, 0);
    assert_eq!(store.get(id).unwrap().yes_size_at(55), Some(10));
}

/// A level update carries no sequence number, so it must neither advance nor
/// disturb the expectation a snapshot set.
#[test]
fn a_level_set_leaves_sequence_continuity_alone() {
    let mut store = polymarket_store();
    let id = ContractId(0);
    store.apply(&snapshot_with(id, 7, &[(55, 10)], &[]));
    assert_eq!(store.expected_seq(Venue::Polymarket), Some(8));

    store.apply(&FeedEvent::LevelSet {
        contract: id,
        side: TokenSide::Bid,
        price: 55,
        size: 99,
        venue_ts_ms: 1_700,
    });
    assert_eq!(store.expected_seq(Venue::Polymarket), Some(8));
    assert_eq!(store.get(id).unwrap().state, BookState::Live);
}

fn snapshot_with(
    contract: ContractId,
    seq: u64,
    yes: &[(i64, i64)],
    no: &[(i64, i64)],
) -> FeedEvent {
    FeedEvent::Snapshot {
        contract,
        yes: yes
            .iter()
            .map(|(price, size)| Level {
                price: *price,
                size: *size,
            })
            .collect(),
        no: no
            .iter()
            .map(|(price, size)| Level {
                price: *price,
                size: *size,
            })
            .collect(),
        seq,
        venue_ts_ms: None,
    }
}
