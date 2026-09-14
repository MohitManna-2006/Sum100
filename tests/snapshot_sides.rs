//! Kalshi omits the key of an empty snapshot side for far strikes. Phase 2
//! rejected those snapshots; the rejected messages still consumed subscription
//! sequence numbers, so every later delta raised a false gap and a forced
//! reconnect (227 in 7 minutes in phase 3 session D).
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use sum100::{
    book::{Applied, BookStore},
    clock::{Clock, ReplayClock},
    feed::{
        Feed, FeedEvent,
        kalshi::Parser,
        replay::{ReplayFeed, ReplayOptions},
    },
    record::read_records,
    types::{Book, BookState, Level, Venue},
    verify::GapLog,
};

const FIXTURE: &str = "phase3-d-one-sided-snapshots.ndjson.gz";

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(FIXTURE)
}

/// Raw snapshot payloads from the fixture, keyed by ticker.
fn raw_snapshots() -> BTreeMap<String, serde_json::Value> {
    read_records(&fixture_path())
        .unwrap()
        .map(Result::unwrap)
        .filter(|record| record.kind == "text")
        .map(|record| serde_json::from_str::<serde_json::Value>(&record.raw).unwrap())
        .filter(|value| value["type"] == "orderbook_snapshot")
        .map(|value| {
            (
                value["msg"]["market_ticker"].as_str().unwrap().to_owned(),
                value,
            )
        })
        .collect()
}

fn side_is_empty(book: &Book, yes: bool) -> bool {
    (0..=100).all(|price| {
        let size = if yes {
            book.yes_size_at(price)
        } else {
            book.no_size_at(price)
        };
        size == Some(0)
    })
}

#[tokio::test]
async fn one_sided_snapshots_from_session_d_stay_live_without_false_gaps() {
    let raw = raw_snapshots();
    let absent = |key: &str| -> BTreeSet<String> {
        raw.iter()
            .filter(|(_, value)| value["msg"].get(key).is_none())
            .map(|(ticker, _)| ticker.clone())
            .collect()
    };
    let (no_absent, yes_absent) = (absent("no_dollars_fp"), absent("yes_dollars_fp"));
    assert_eq!((raw.len(), no_absent.len(), yes_absent.len()), (80, 31, 20));
    assert!(no_absent.is_disjoint(&yes_absent));

    let mut feed = ReplayFeed::open(&fixture_path(), ReplayOptions::default()).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(feed.clock());
    let mut store = BookStore::new(Venue::Kalshi, feed.tickers(), clock).unwrap();
    let mut gaps = GapLog::default();
    let mut last_seq = 0;
    let mut far_deltas = 0;
    while let Some(event) = feed.next().await {
        let applied = gaps.apply(&mut store, &event);
        match (&event, applied) {
            (FeedEvent::Snapshot { contract, seq, .. }, Applied::Snapshot(id)) => {
                assert_eq!(*contract, id);
                last_seq = *seq;
                let ticker = &store.contracts().resolve(id).unwrap().1;
                let book = store.get(id).unwrap();
                assert_eq!((book.state, book.seq), (BookState::Live, *seq));
                assert_eq!(store.expected_seq(), Some(seq + 1));
                // Empty as applied, before any later delta can add a level.
                if no_absent.contains(ticker) {
                    assert!(side_is_empty(book, false), "{ticker} no side");
                    assert_eq!(book.best_no_bid(), None);
                    assert_eq!(book.asks().count(), 0);
                    assert!(book.best_bid().is_some(), "{ticker} yes side present");
                }
                if yes_absent.contains(ticker) {
                    assert!(side_is_empty(book, true), "{ticker} yes side");
                    assert_eq!(book.best_bid(), None);
                    assert!(book.best_no_bid().is_some(), "{ticker} no side present");
                }
            }
            (FeedEvent::Delta { seq, .. }, Applied::Delta(id)) => {
                assert_eq!(*seq, last_seq + 1, "sequence advances by one");
                last_seq = *seq;
                let ticker = &store.contracts().resolve(id).unwrap().1;
                if no_absent.contains(ticker) || yes_absent.contains(ticker) {
                    far_deltas += 1;
                }
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    let metrics = feed.finish().unwrap();
    assert_eq!(metrics.parse_errors, 0);
    assert_eq!(metrics.snapshot_sides_absent, 51);
    assert_eq!(last_seq, 153);
    assert_eq!(store.expected_seq(), Some(154));
    assert_eq!(store.metrics.snapshots_applied, 80);
    assert_eq!(store.metrics.deltas_applied, 73);
    assert_eq!(far_deltas, 70);
    assert_eq!(store.metrics.sequence_gaps, 0);
    assert_eq!(store.metrics.resync_requests, 0);
    assert_eq!(store.metrics.deltas_skipped_not_live, 0);
    assert!(gaps.entries().is_empty(), "{:?}", gaps.entries());
    assert!(
        store
            .books()
            .iter()
            .all(|book| book.state == BookState::Live)
    );
}

#[test]
fn malformed_snapshots_are_still_rejected_and_counted() {
    let raw = raw_snapshots();
    let (ticker, one_sided) = raw
        .iter()
        .find(|(_, value)| value["msg"].get("no_dollars_fp").is_none())
        .map(|(ticker, value)| (ticker.clone(), value.clone()))
        .unwrap();
    let mut parser = Parser::new(std::slice::from_ref(&ticker)).unwrap();

    // Control: the captured one-sided payload is accepted, not counted as an error.
    assert!(matches!(
        parser.parse(&one_sided.to_string(), 0),
        Some(FeedEvent::Snapshot { .. })
    ));
    assert_eq!(
        (
            parser.metrics.parse_errors,
            parser.metrics.snapshot_sides_absent
        ),
        (0, 1)
    );

    // Deliberate mutations of that capture; none of these is a venue payload.
    type Mutation = fn(&mut serde_json::Value);
    let malformed: [(&str, Mutation); 8] = [
        ("side is a string", |v| {
            v["msg"]["no_dollars_fp"] = "0.55".into()
        }),
        ("side is an object", |v| {
            v["msg"]["no_dollars_fp"] = serde_json::json!({})
        }),
        ("row is not a pair", |v| {
            v["msg"]["yes_dollars_fp"] = serde_json::json!([["0.0100"]])
        }),
        ("row values are numbers", |v| {
            v["msg"]["yes_dollars_fp"] = serde_json::json!([[0.01, 5]])
        }),
        ("sub-cent price", |v| {
            v["msg"]["yes_dollars_fp"] = serde_json::json!([["0.005", "5.00"]])
        }),
        ("both sides absent", |v| {
            v["msg"].as_object_mut().unwrap().remove("yes_dollars_fp");
        }),
        ("missing ticker", |v| {
            v["msg"].as_object_mut().unwrap().remove("market_ticker");
        }),
        ("missing seq", |v| {
            v.as_object_mut().unwrap().remove("seq");
        }),
    ];
    for (i, (name, mutate)) in malformed.iter().enumerate() {
        let mut payload = one_sided.clone();
        mutate(&mut payload);
        assert_eq!(parser.parse(&payload.to_string(), 0), None, "{name}");
        assert_eq!(parser.metrics.parse_errors, i as u64 + 1, "{name}");
        assert_eq!(parser.metrics.snapshot_sides_absent, 1, "{name}");
    }
}

#[test]
fn absent_side_clears_stale_levels_and_null_is_not_counted_absent() {
    let raw = raw_snapshots();
    let (ticker, one_sided) = raw
        .iter()
        .find(|(_, value)| value["msg"].get("no_dollars_fp").is_none())
        .map(|(ticker, value)| (ticker.clone(), value.clone()))
        .unwrap();
    let tickers = vec![ticker];
    let mut parser = Parser::new(&tickers).unwrap();
    let mut store = BookStore::new(Venue::Kalshi, &tickers, Arc::new(ReplayClock::new())).unwrap();

    // An earlier two-sided book, as before a reconnect.
    let FeedEvent::Snapshot { contract, .. } = parser.parse(&one_sided.to_string(), 0).unwrap()
    else {
        panic!("snapshot");
    };
    let stale = FeedEvent::Snapshot {
        contract,
        yes: vec![Level { price: 3, size: 7 }],
        no: vec![Level {
            price: 95,
            size: 40,
        }],
        seq: 1,
        venue_ts_ms: None,
    };
    store.apply(&stale);
    assert_eq!(store.get(contract).unwrap().no_size_at(95), Some(40));
    store.apply(&FeedEvent::Disconnected {
        venue: Venue::Kalshi,
    });

    // The captured one-sided snapshot replaces the book: the absent no side
    // reads empty, not the stale 40 at 95.
    let event = parser.parse(&one_sided.to_string(), 0).unwrap();
    assert_eq!(store.apply(&event), Applied::Snapshot(contract));
    let book = store.get(contract).unwrap();
    assert_eq!(book.state, BookState::Live);
    assert!(side_is_empty(book, false));
    assert_eq!(book.yes_size_at(3), Some(0));
    assert!(book.best_bid().is_some());

    // Explicit null is also empty, but it is not an absent key.
    let mut null_side = one_sided.clone();
    null_side["msg"]["no_dollars_fp"] = serde_json::Value::Null;
    let before = parser.metrics.snapshot_sides_absent;
    let Some(FeedEvent::Snapshot { no, .. }) = parser.parse(&null_side.to_string(), 0) else {
        panic!("null side should parse");
    };
    assert!(no.is_empty());
    assert_eq!(parser.metrics.snapshot_sides_absent, before);
    assert_eq!(parser.metrics.parse_errors, 0);

    // The fixture is gzip; confirm it is the checked-in file, not a rewrite.
    let mut bytes = Vec::new();
    std::fs::File::open(fixture_path())
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes.len(), 11_836);
}
