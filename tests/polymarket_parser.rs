//! Polymarket market-channel parsing, against a recorded production session.
//!
//! The fixture is 60 seconds of `wss://ws-subscriptions-clob.polymarket.com/ws/market`
//! on a 0.01-tick market, stored verbatim. Every claim the parser is built on —
//! that a level update carries an absolute size, that the two tokens are exact
//! complements, that the heartbeat reply is not JSON — is asserted here against
//! those bytes rather than against a payload written to match the parser.

use flate2::read::GzDecoder;
use std::{collections::HashMap, io::Read, path::Path};
use sum100::{
    feed::polymarket::{Parsed, Parser, TokenSide},
    types::Venue,
};

/// The two `clobTokenIds` of the recorded market, yes first.
const YES: &str = "61682588409713156892865066024379723903051700030517382076759724382208757063880";
const NO: &str = "77581328830895265733192791046346830606134150191771311968443897212492955009642";

#[derive(serde::Deserialize)]
struct Envelope {
    received_at_ms: u64,
    kind: String,
    raw: String,
}

fn envelopes() -> Vec<Envelope> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/polymarket-0.01-tick-sample.ndjson.gz");
    let mut text = String::new();
    GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn parser() -> Parser {
    Parser::new(&[YES.to_owned(), NO.to_owned()]).unwrap()
}

/// The recording is what the parser is specified against, so its shape is
/// pinned: any drift means the fixture was replaced, not that the code changed.
#[test]
fn the_recorded_session_has_the_shape_the_parser_is_written_for() {
    let records = envelopes();
    assert_eq!(records.len(), 80);
    assert_eq!(records.iter().filter(|r| r.kind == "control").count(), 2);
    assert_eq!(records.iter().filter(|r| r.kind == "text").count(), 78);
}

/// Every text frame in a real session must parse or be knowingly ignored. A
/// single parse error here means the wire format is not what the parser thinks.
#[test]
fn every_recorded_frame_parses_without_error() {
    let mut parser = parser();
    let (mut books, mut level_frames, mut ignored, mut levels) = (0, 0, 0, 0);

    for record in envelopes().iter().filter(|r| r.kind == "text") {
        match parser.parse(&record.raw, record.received_at_ms) {
            Some(Parsed::Books(snapshots)) => {
                // One frame, one entry per subscribed token.
                assert_eq!(snapshots.len(), 2);
                books += 1;
            }
            Some(Parsed::Levels { changes, .. }) => {
                level_frames += 1;
                levels += changes.len();
            }
            None => ignored += 1,
        }
    }

    assert_eq!(books, 1, "one book snapshot frame");
    assert_eq!(level_frames, 72, "price_change frames");
    assert_eq!(levels, 144, "level updates across those frames");
    assert_eq!(ignored, 5, "the PONG heartbeat replies");
    assert_eq!(parser.metrics.parse_errors, 0);
    assert_eq!(parser.metrics.unknown_messages, 0);
    assert_eq!(parser.metrics.parse_attempts, 78);
}

/// The heartbeat reply is a bare token, not JSON. Counting it as a rejected
/// payload would report a standing parse-error rate on a healthy feed.
#[test]
fn the_pong_heartbeat_is_ignored_rather_than_rejected() {
    let mut parser = parser();
    assert!(parser.parse("PONG", 1_000).is_none());
    assert!(parser.parse("PING", 1_000).is_none());
    assert_eq!(parser.metrics.parse_errors, 0);

    // A payload that genuinely is not understood still counts.
    assert!(parser.parse("{not json", 1_000).is_none());
    assert_eq!(parser.metrics.parse_errors, 1);
}

/// The snapshot carries full ladders, not a single top-of-book quote, and both
/// sides are populated.
#[test]
fn the_book_snapshot_carries_both_full_ladders() {
    let mut parser = parser();
    let record = envelopes()
        .into_iter()
        .find(|r| r.kind == "text" && r.raw.starts_with('['))
        .expect("book frame");
    let Some(Parsed::Books(snapshots)) = parser.parse(&record.raw, record.received_at_ms) else {
        panic!("expected a book frame");
    };

    let yes = snapshots
        .iter()
        .find(|s| s.contract == parser.contracts.get(Venue::Polymarket, YES).unwrap())
        .expect("yes token snapshot");
    assert_eq!(yes.bids.len(), 31);
    assert_eq!(yes.asks.len(), 26);
    assert!(!yes.hash.is_empty());

    // Prices are cents in the band a binary contract can occupy, and sizes are
    // whole contracts floored from the venue's fractional shares.
    for level in yes.bids.iter().chain(yes.asks.iter()) {
        assert!((0..=100).contains(&level.price), "price {}", level.price);
        assert!(level.size > 0, "size {}", level.size);
    }

    let best_bid = yes.bids.iter().map(|l| l.price).max().unwrap();
    let best_ask = yes.asks.iter().map(|l| l.price).min().unwrap();
    assert_eq!(best_bid, 55);
    assert_eq!(best_ask, 56);
    assert!(best_bid < best_ask, "a healthy book is not crossed");
}

/// The claim the whole parser rests on: a level update replaces the size at a
/// price, it does not adjust it. The recording pins one level across the
/// snapshot and the update that follows it.
#[test]
fn a_level_update_states_the_new_size_rather_than_a_change() {
    let mut parser = parser();
    let yes_id = parser.contracts.get(Venue::Polymarket, YES).unwrap();
    let mut asks: HashMap<i64, i64> = HashMap::new();
    let mut snapshot_57 = None;
    let mut cumulative_57 = 0i64;
    let mut replacements = 0u32;

    for record in envelopes().iter().filter(|r| r.kind == "text") {
        match parser.parse(&record.raw, record.received_at_ms) {
            Some(Parsed::Books(snapshots)) => {
                for snapshot in snapshots.iter().filter(|s| s.contract == yes_id) {
                    for level in &snapshot.asks {
                        asks.insert(level.price, level.size);
                        if level.price == 57 {
                            snapshot_57 = Some(level.size);
                            cumulative_57 = level.size;
                        }
                    }
                }
            }
            Some(Parsed::Levels { changes, .. }) => {
                for change in changes {
                    if change.contract != yes_id || change.side != TokenSide::Ask {
                        continue;
                    }
                    if change.price == 57 {
                        cumulative_57 = cumulative_57.saturating_add(change.size);
                    }
                    if let Some(previous) = asks.insert(change.price, change.size) {
                        // Under a delta reading this would be the amount to add,
                        // and a level would routinely be handed its own size.
                        assert_ne!(
                            change.size, previous,
                            "a level was reassigned its existing size at {}c",
                            change.price
                        );
                        replacements += 1;
                    }
                }
            }
            None => {}
        }
    }

    assert!(
        replacements >= 20,
        "expected repeated updates to known levels, saw {replacements}"
    );

    // The 57c ask moved, so the updates were not no-ops, and it stayed within
    // an order of magnitude of where it started. Adding the updates instead —
    // the delta reading — inflates the level far beyond any size the venue
    // reported, which is how the two readings are told apart.
    let snapshot_57 = snapshot_57.expect("the snapshot quoted a 57c ask");
    let final_57 = asks[&57];
    assert_ne!(final_57, snapshot_57);
    assert!(
        final_57 < snapshot_57 * 2,
        "absolute reading keeps the level plausible: {snapshot_57} -> {final_57}"
    );
    assert!(
        cumulative_57 > snapshot_57 * 4,
        "a delta reading would have inflated it to {cumulative_57}"
    );
}

/// The two tokens are one market reported twice. Every update names both, at
/// complementary prices and identical size, which is what lets one token's two
/// ladders stand in for the whole book.
#[test]
fn the_two_tokens_are_exact_complements_of_each_other() {
    let mut parser = parser();
    let yes_id = parser.contracts.get(Venue::Polymarket, YES).unwrap();
    let no_id = parser.contracts.get(Venue::Polymarket, NO).unwrap();
    let mut pairs = 0u32;

    for record in envelopes().iter().filter(|r| r.kind == "text") {
        let Some(Parsed::Levels { changes, .. }) = parser.parse(&record.raw, record.received_at_ms)
        else {
            continue;
        };
        let yes = changes.iter().find(|c| c.contract == yes_id);
        let no = changes.iter().find(|c| c.contract == no_id);
        let (Some(yes), Some(no)) = (yes, no) else {
            continue;
        };
        assert_eq!(yes.price + no.price, 100, "prices are complements");
        assert_eq!(yes.size, no.size, "one resting order, reported twice");
        // A bid on one token is an ask on the other.
        assert_ne!(yes.side, no.side);
        pairs += 1;
    }

    assert_eq!(pairs, 72, "every frame named both tokens");
}

/// A token the caller did not subscribe to is skipped, not rejected: one
/// market's frame names both its tokens and only one need be carried.
#[test]
fn an_unsubscribed_token_in_a_frame_is_skipped_quietly() {
    let mut parser = Parser::new(&[YES.to_owned()]).unwrap();
    let mut frames = 0u32;
    for record in envelopes().iter().filter(|r| r.kind == "text") {
        if let Some(Parsed::Levels { changes, .. }) =
            parser.parse(&record.raw, record.received_at_ms)
        {
            assert_eq!(changes.len(), 1, "only the subscribed token survives");
            frames += 1;
        }
    }
    assert_eq!(frames, 72);
    assert_eq!(parser.metrics.parse_errors, 0);
}

/// A tick finer than a cent is refused rather than rounded. These markets exist
/// on Polymarket, and a rounded quote is one nobody made.
///
/// It is counted apart from a parse error, because the payload was understood
/// perfectly: the price is one a whole-cent book has no slot for. Reporting it
/// as malformed data would send an operator looking for a broken parser.
#[test]
fn a_sub_cent_price_is_refused_and_counted_as_its_own_thing() {
    let mut parser = parser();
    let frame = format!(
        r#"{{"event_type":"price_change","timestamp":"1789921463565","price_changes":[{{"asset_id":"{YES}","price":"0.085","size":"100","side":"BUY","hash":"h"}}]}}"#
    );
    assert!(parser.parse(&frame, 1_000).is_none());
    assert_eq!(parser.metrics.sub_cent_prices, 1);
    assert_eq!(parser.metrics.parse_errors, 0, "understood, not malformed");
}

/// One frame carries both tokens. A price the book cannot hold costs its own
/// level, not its sibling's: dropping the whole frame would discard a
/// perfectly representable quote alongside the one that could not be used.
#[test]
fn an_unrepresentable_price_does_not_take_its_sibling_down_with_it() {
    let mut parser = parser();
    let frame = format!(
        r#"{{"event_type":"price_change","timestamp":"1789921463565","price_changes":[
            {{"asset_id":"{YES}","price":"0.085","size":"100","side":"BUY","hash":"a"}},
            {{"asset_id":"{NO}","price":"0.92","size":"250","side":"SELL","hash":"b"}}
        ]}}"#
    );
    let Some(Parsed::Levels { changes, .. }) = parser.parse(&frame, 1_000) else {
        panic!("the representable half should still arrive");
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].price, 92);
    assert_eq!(changes[0].size, 250);
    assert_eq!(parser.metrics.sub_cent_prices, 1);
    assert_eq!(parser.metrics.parse_errors, 0);
}

/// Informational message types are known and carry no book state, so they are
/// ignored without being counted as payloads the parser did not recognize.
#[test]
fn trade_prints_and_tick_changes_are_known_and_ignored() {
    let mut parser = parser();
    let trade = format!(
        r#"{{"event_type":"last_trade_price","asset_id":"{YES}","price":"0.55","size":"10","side":"BUY","timestamp":"1789921463565"}}"#
    );
    let tick = format!(
        r#"{{"event_type":"tick_size_change","asset_id":"{YES}","old_tick_size":"0.01","new_tick_size":"0.001","timestamp":"1789921463565"}}"#
    );
    assert!(parser.parse(&trade, 1_000).is_none());
    assert!(parser.parse(&tick, 1_000).is_none());
    assert_eq!(parser.metrics.parse_errors, 0);
    assert_eq!(parser.metrics.unknown_messages, 0);

    let unknown = r#"{"event_type":"something_new","timestamp":"1"}"#;
    assert!(parser.parse(unknown, 1_000).is_none());
    assert_eq!(parser.metrics.unknown_messages, 1);
    assert_eq!(parser.metrics.parse_errors, 0);
}

/// Fractional shares floor to whole contracts, and the remainder is counted
/// rather than lost silently — the same accounting the Kalshi path uses.
#[test]
fn fractional_sizes_floor_with_an_exact_discard_counter() {
    let mut parser = parser();
    let frame = format!(
        r#"{{"event_type":"price_change","timestamp":"1789921463565","price_changes":[{{"asset_id":"{YES}","price":"0.55","size":"100.45","side":"BUY","hash":"h"}}]}}"#
    );
    let Some(Parsed::Levels { changes, .. }) = parser.parse(&frame, 1_000) else {
        panic!("expected a level update");
    };
    assert_eq!(changes[0].size, 100);
    assert_eq!(changes[0].side, TokenSide::Bid);
    assert_eq!(changes[0].price, 55);
    assert_eq!(parser.metrics.discarded_size_hundredths, 45);
}

/// Venue timestamps feed the latency histogram, as they do for Kalshi, so a
/// slow feed is visible on the dashboard rather than inferred.
#[test]
fn venue_timestamps_are_observed_as_latency() {
    let mut parser = parser();
    for record in envelopes().iter().filter(|r| r.kind == "text") {
        parser.parse(&record.raw, record.received_at_ms);
    }
    assert_eq!(
        parser.metrics.latency_samples(),
        72,
        "one sample per price_change frame"
    );
}

/// The whole recorded session, driven through the real book store.
///
/// This is the claim that matters end to end: a Polymarket market's two ladders
/// land in the same `Book` a Kalshi contract uses, the book goes live on the
/// snapshot and stays live, and the derived ask ladder reads back at the prices
/// the venue actually quoted.
#[test]
fn the_recorded_session_drives_a_book_store_to_a_live_book() {
    use std::sync::Arc;
    use sum100::{
        book::{Applied, BookStore},
        clock::ReplayClock,
        types::BookState,
    };

    let tokens = vec![YES.to_owned()];
    let clock = ReplayClock::new();
    let mut store =
        BookStore::multi_venue(&[(Venue::Polymarket, &tokens)], Arc::new(clock.clone())).unwrap();
    let mut parser = Parser::new(&tokens).unwrap();
    let (mut snapshots, mut levels, mut skipped) = (0u32, 0u32, 0u32);
    let snapshot_count = std::cell::Cell::new(0u64);

    for record in envelopes().iter().filter(|r| r.kind == "text") {
        clock.advance_to(record.received_at_ms);
        let Some(parsed) = parser.parse(&record.raw, record.received_at_ms) else {
            continue;
        };
        for event in parsed.into_events(|_| {
            snapshot_count.set(snapshot_count.get() + 1);
            snapshot_count.get()
        }) {
            match store.apply(&event) {
                Applied::Snapshot(_) => snapshots += 1,
                Applied::LevelSet(_) => levels += 1,
                Applied::Skipped => skipped += 1,
                other => panic!("unexpected outcome {other:?}"),
            }
        }
    }

    assert_eq!(snapshots, 1, "one snapshot, for the one subscribed token");
    assert_eq!(levels, 72, "one level update per frame for that token");
    assert_eq!(skipped, 0, "every update landed");
    assert_eq!(store.metrics.levels_replaced, 72);
    assert_eq!(store.metrics.sequence_gaps, 0);

    let book = store.get(sum100::types::ContractId(0)).unwrap();
    assert_eq!(book.state, BookState::Live);
    assert_eq!(book.venue, Venue::Polymarket);

    // The ladders read back as a coherent two-sided book at the venue's prices.
    let best_bid = book.best_bid().expect("a resting bid");
    let best_ask = book.best_ask().expect("a resting ask");
    assert!(
        best_bid.price < best_ask.price,
        "book is not crossed: {} / {}",
        best_bid.price,
        best_ask.price
    );
    assert!((0..=100).contains(&best_bid.price));
    assert!((0..=100).contains(&best_ask.price));
}

/// Contract ids are positional, so every feed in a run must intern every
/// venue's identifiers in the same order the book store did.
///
/// A parser that interned only its own tokens would number them from zero and
/// address another venue's books: a Polymarket level update would land on a
/// Kalshi contract, cross it, and read out as a complement arbitrage that does
/// not exist. That is not hypothetical — it produced 4,525 false opportunities
/// and 1,051 crossed books in a live run before this was pinned.
#[test]
fn a_parser_agrees_with_a_multi_venue_book_store_on_ids() {
    use std::sync::Arc;
    use sum100::{book::BookStore, clock::ReplayClock, types::ContractId};

    let kalshi: Vec<String> = vec!["KXA".into(), "KXB".into(), "KXC".into()];
    let poly: Vec<String> = vec![YES.to_owned(), NO.to_owned()];
    let subscriptions: [(Venue, &[String]); 2] =
        [(Venue::Kalshi, &kalshi), (Venue::Polymarket, &poly)];

    let store = BookStore::multi_venue(&subscriptions, Arc::new(ReplayClock::new())).unwrap();
    let parser = Parser::across(&subscriptions).unwrap();

    // The Polymarket tokens come after the Kalshi ones, not from zero.
    let yes_id = parser.contracts.get(Venue::Polymarket, YES).unwrap();
    assert_eq!(yes_id, ContractId(3));
    assert_eq!(
        parser.contracts.get(Venue::Polymarket, NO).unwrap(),
        ContractId(4)
    );

    // And every id the parser can emit resolves to a book of the right venue.
    for token in &poly {
        let id = parser.contracts.get(Venue::Polymarket, token).unwrap();
        let book = store.get(id).expect("a book for every subscribed token");
        assert_eq!(book.venue, Venue::Polymarket);
        assert_eq!(store.contracts().resolve(id), parser.contracts.resolve(id));
    }
    // The Kalshi slots belong to Kalshi, which is what was being overwritten.
    for (index, ticker) in kalshi.iter().enumerate() {
        let id = ContractId(index as u32);
        assert_eq!(store.get(id).unwrap().venue, Venue::Kalshi);
        assert_eq!(
            store.contracts().resolve(id),
            Some(&(Venue::Kalshi, ticker.clone()))
        );
    }
}
