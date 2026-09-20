use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};
use sum100::{
    book::BookStore,
    clock::Clock,
    feed::{
        Feed, FeedEvent,
        kalshi::Parser,
        replay::{Pace, ReplayFeed, ReplayOptions, sessions},
    },
    record::{CONTROL_KIND, Control, Record, Recorder, read_records},
    types::{BookState, ContractId, Venue},
    verify::{Expected, GapLog, StateDigest},
};

const TICKER: &str = "KXBTCD-26SEP1417-T76999.99";
/// 2026-09-13T23:44:10Z; every synthetic receipt time stays on this UTC day.
const BASE_MS: u64 = 1_789_343_050_000;

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

fn temp() -> PathBuf {
    let path = std::env::temp_dir().join(format!("sum100-replay-{}", rand::random::<u64>()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn tickers() -> Vec<String> {
    vec![TICKER.to_owned()]
}

enum Entry {
    Text(u64, String),
    Control(u64, Control),
}

/// Write entries through the real recorder and return the single daily file.
fn record(dir: &Path, entries: &[Entry]) -> PathBuf {
    let mut recorder = Recorder::new(dir, Venue::Kalshi).unwrap();
    for entry in entries {
        match entry {
            Entry::Text(ts, raw) => recorder.write(*ts, "text", raw).unwrap(),
            Entry::Control(ts, control) => recorder.write_control(*ts, control).unwrap(),
        };
    }
    recorder.flush().unwrap();
    let files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".ndjson.gz"))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files.into_iter().next().unwrap()
}

fn session_marker(ts: u64) -> Entry {
    Entry::Control(
        ts,
        Control::SessionStarted {
            environment: "production".into(),
            tickers: tickers(),
        },
    )
}

struct Run {
    events: Vec<FeedEvent>,
    store: BookStore,
    gaps: GapLog,
    digest: StateDigest,
    clock_ms: u64,
    parse_errors: u64,
}

async fn replay(path: &Path, options: ReplayOptions) -> Run {
    let mut feed = ReplayFeed::open(path, options).unwrap();
    let clock = feed.clock();
    let mut store = BookStore::new(Venue::Kalshi, feed.tickers(), Arc::new(clock.clone())).unwrap();
    let mut gaps = GapLog::default();
    let mut events = Vec::new();
    while let Some(event) = feed.next().await {
        // Every event is applied at its own receipt time.
        gaps.apply(&mut store, &event);
        events.push(event);
    }
    let metrics = feed.finish().unwrap();
    let digest = StateDigest::new(&store, &gaps);
    Run {
        events,
        store,
        gaps,
        digest,
        clock_ms: clock.now_ms(),
        parse_errors: metrics.parse_errors,
    }
}

fn max(tickers: Option<Vec<String>>) -> ReplayOptions {
    ReplayOptions {
        tickers,
        pace: Pace::Max,
        session: None,
    }
}

#[tokio::test]
async fn legacy_file_replays_through_live_parse_path_to_pinned_book() {
    let frames = fixture("stage2-live-orderbook.ndjson");
    let dir = temp();
    // Recorded exactly as the pre-phase-3 recorder wrote it: no control envelopes.
    let entries: Vec<Entry> = frames
        .iter()
        .enumerate()
        .map(|(i, raw)| Entry::Text(BASE_MS + 150 * i as u64, raw.clone()))
        .collect();
    let path = record(&dir, &entries);

    let error = ReplayFeed::open(&path, max(None)).err().unwrap();
    assert!(error.to_string().contains("--tickers"), "{error}");

    let run = replay(&path, max(Some(tickers()))).await;
    // Same events, in order, as calling the live parser on the same bytes.
    let mut parser = Parser::new(&tickers()).unwrap();
    let direct: Vec<FeedEvent> = frames
        .iter()
        .enumerate()
        .filter_map(|(i, raw)| parser.parse(raw, BASE_MS + 150 * i as u64))
        .collect();
    assert_eq!(run.events, direct);
    assert_eq!(run.events.len(), 461);
    assert_eq!(run.parse_errors, 0);

    // Final state pinned in phase 2 (tests/book.rs).
    let book = run.store.get(ContractId(0)).unwrap();
    assert_eq!(book.state, BookState::Live);
    assert_eq!(book.seq, 461);
    assert_eq!(book.best_bid().map(|l| (l.price, l.size)), Some((44, 4294)));
    assert_eq!(book.best_ask().map(|l| (l.price, l.size)), Some((46, 100)));
    assert_eq!((book.bids().count(), book.asks().count()), (26, 26));
    assert!(run.gaps.entries().is_empty());

    // Clock followed receipt time, and books were stamped with it rather than
    // with the venue timestamp carried on the delta.
    let last_receipt = BASE_MS + 150 * (frames.len() as u64 - 1);
    assert_eq!(run.clock_ms, last_receipt);
    assert_eq!(book.updated_at_ms, last_receipt);
    let FeedEvent::Delta { venue_ts_ms, .. } = run.events.last().unwrap() else {
        panic!("last event should be a delta");
    };
    assert_ne!(*venue_ts_ms, last_receipt);
    fs::remove_dir_all(dir).unwrap();
}

/// A live session with a real sequence gap followed by the forced reconnect,
/// written in the exact order `KalshiFeed` records and emits.
fn resync_session(with_controls: bool) -> (Vec<Entry>, Vec<FeedEvent>) {
    let frames = fixture("stage2-live-orderbook.ndjson");
    let mut parser = Parser::new(&tickers()).unwrap();
    let mut entries = Vec::new();
    let mut live = Vec::new();
    let mut ts = BASE_MS;
    let mut text = |entries: &mut Vec<Entry>, live: &mut Vec<FeedEvent>, raw: &str, ts: u64| {
        entries.push(Entry::Text(ts, raw.to_owned()));
        live.extend(parser.parse(raw, ts));
    };
    if with_controls {
        entries.push(session_marker(ts));
    }
    // ack, snapshot seq 1, deltas seq 2..=3; seq 4 lost; seq 5..=7 arrive.
    for (i, raw) in frames.iter().enumerate().take(8) {
        ts += 100;
        if i != 4 {
            text(&mut entries, &mut live, raw, ts);
        }
    }
    ts += 100;
    if with_controls {
        entries.push(Entry::Control(
            ts,
            Control::Disconnected {
                reason: "resync requested".into(),
            },
        ));
        live.push(FeedEvent::Disconnected {
            venue: Venue::Kalshi,
        });
        entries.push(Entry::Control(
            ts + 250,
            Control::Reconnected { attempt: 0 },
        ));
    }
    ts += 400;
    text(&mut entries, &mut live, &frames[0], ts); // subscription ack
    if with_controls {
        entries.push(Entry::Control(
            ts,
            Control::Resubscribed { tickers: tickers() },
        ));
        live.push(FeedEvent::Resubscribed {
            contract: ContractId(0),
        });
    }
    // New subscription: fresh snapshot at seq 1, then consecutive deltas.
    for raw in frames.iter().skip(1) {
        ts += 100;
        text(&mut entries, &mut live, raw, ts);
    }
    (entries, live)
}

#[tokio::test]
async fn control_envelopes_reproduce_live_gap_and_resync_path() {
    let (entries, live_events) = resync_session(true);
    let dir = temp();
    let path = record(&dir, &entries);
    let run = replay(&path, max(None)).await;
    assert_eq!(run.events, live_events);

    // What the live consumer computed from the events it received.
    let mut store = BookStore::new(
        Venue::Kalshi,
        &tickers(),
        Arc::new(sum100::clock::ReplayClock::new()),
    )
    .unwrap();
    let mut gaps = GapLog::default();
    for event in &live_events {
        gaps.apply(&mut store, event);
    }
    assert_eq!(run.digest, StateDigest::new(&store, &gaps));
    assert_eq!(
        run.gaps.entries(),
        [
            format!("4 gap {TICKER} expected=4 got=5"),
            "7 disconnected kalshi".to_owned(),
            format!("8 resubscribed {TICKER}"),
            format!("9 resynced {TICKER} seq=1"),
        ]
    );
    assert_eq!(run.store.metrics.sequence_gaps, 1);
    assert_eq!(run.store.metrics.resync_requests, 1);
    assert_eq!(run.store.metrics.deltas_skipped_not_live, 2);
    let book = run.store.get(ContractId(0)).unwrap();
    assert_eq!((book.state, book.seq), (BookState::Live, 461));
    assert_eq!(book.best_bid().map(|l| (l.price, l.size)), Some((44, 4294)));

    // Without control envelopes the same venue bytes take a different path:
    // the disconnect and resubscribe never happen, so the gap log diverges.
    let (legacy, _) = resync_session(false);
    let legacy_dir = temp();
    let legacy_run = replay(&record(&legacy_dir, &legacy), max(Some(tickers()))).await;
    assert_ne!(legacy_run.digest.gaps, run.digest.gaps);
    assert_eq!(
        legacy_run.gaps.entries(),
        [
            format!("4 gap {TICKER} expected=4 got=5"),
            format!("7 resynced {TICKER} seq=1"),
        ]
    );
    fs::remove_dir_all(dir).unwrap();
    fs::remove_dir_all(legacy_dir).unwrap();
}

#[tokio::test]
async fn digest_is_repeatable_and_excludes_timestamps() {
    let (entries, _) = resync_session(true);
    let shifted: Vec<Entry> = entries
        .iter()
        .map(|entry| match entry {
            Entry::Text(ts, raw) => Entry::Text(ts + 3_600_000, raw.clone()),
            Entry::Control(ts, control) => Entry::Control(ts + 3_600_000, control.clone()),
        })
        .collect();
    let (a, b, c) = (temp(), temp(), temp());
    let path = record(&a, &entries);
    let first = replay(&path, max(None)).await;
    let second = replay(&path, max(None)).await;
    let moved = replay(&record(&b, &shifted), max(None)).await;
    assert_eq!(first.digest, second.digest);
    assert_eq!(first.digest.render(), moved.digest.render());
    assert_ne!(first.clock_ms, moved.clock_ms);

    // The rendered digest round-trips as a verification input.
    let expected = Expected::parse(&first.digest.render()).unwrap();
    assert!(expected.check(&moved.digest).is_empty());
    let mut wrong = expected.clone();
    wrong.books.insert(TICKER.into(), "0".repeat(64));
    assert_eq!(wrong.check(&moved.digest).len(), 1);
    assert_eq!(Expected::default().check(&moved.digest).len(), 2);

    // A level change is visible in the book hash.
    let frames = fixture("stage2-live-orderbook.ndjson");
    let truncated: Vec<Entry> = frames[..frames.len() - 1]
        .iter()
        .enumerate()
        .map(|(i, raw)| Entry::Text(BASE_MS + i as u64, raw.clone()))
        .collect();
    let short = replay(&record(&c, &truncated), max(Some(tickers()))).await;
    assert_ne!(short.digest.books, first.digest.books);
    for dir in [a, b, c] {
        fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn sessions_in_one_daily_file_are_separated() {
    let frames = fixture("stage2-live-orderbook.ndjson");
    let mut entries = Vec::new();
    // Unmarked prefix (older recorder), then two marked runs appended later.
    for (i, raw) in frames.iter().take(3).enumerate() {
        entries.push(Entry::Text(BASE_MS + i as u64, raw.clone()));
    }
    entries.push(session_marker(BASE_MS + 1_000));
    for (i, raw) in frames.iter().take(10).enumerate() {
        entries.push(Entry::Text(BASE_MS + 1_001 + i as u64, raw.clone()));
    }
    entries.push(session_marker(BASE_MS + 2_000));
    for (i, raw) in frames.iter().take(20).enumerate() {
        entries.push(Entry::Text(BASE_MS + 2_001 + i as u64, raw.clone()));
    }
    let dir = temp();
    let path = record(&dir, &entries);

    let found = sessions(&path).unwrap();
    assert_eq!(found.len(), 3);
    assert_eq!(found[0].tickers, None);
    assert_eq!(found[1].tickers, Some(tickers()));
    assert_eq!(
        found.iter().map(|s| s.records).collect::<Vec<_>>(),
        [3, 11, 21]
    );

    let error = ReplayFeed::open(&path, max(None)).err().unwrap();
    assert!(error.to_string().contains("3 sessions"), "{error}");
    let pick = |session| ReplayOptions {
        session: Some(session),
        ..max(None)
    };
    assert!(ReplayFeed::open(&path, pick(1)).is_err());
    assert!(ReplayFeed::open(&path, pick(4)).is_err());
    assert!(
        ReplayFeed::open(
            &path,
            ReplayOptions {
                tickers: Some(vec!["OTHER".into()]),
                ..pick(2)
            }
        )
        .is_err()
    );

    let second = replay(&path, pick(2)).await;
    assert_eq!(second.events.len(), 9);
    assert_eq!(second.store.get(ContractId(0)).unwrap().seq, 9);
    let third = replay(&path, pick(3)).await;
    assert_eq!(third.events.len(), 19);
    let legacy = replay(
        &path,
        ReplayOptions {
            tickers: Some(tickers()),
            ..pick(1)
        },
    )
    .await;
    assert_eq!(legacy.events.len(), 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn control_kind_is_the_tag_and_old_envelopes_still_parse() {
    let dir = temp();
    let mut recorder = Recorder::new(&dir, Venue::Kalshi).unwrap();
    assert!(recorder.write(BASE_MS, CONTROL_KIND, "{}").is_err());
    // Venue text that happens to look like a control payload stays venue text.
    recorder
        .write(BASE_MS, "text", r#"{"event":"disconnected","reason":"x"}"#)
        .unwrap();
    let control = Control::Disconnected {
        reason: "websocket idle timeout".into(),
    };
    recorder.write_control(BASE_MS + 1, &control).unwrap();
    recorder.flush().unwrap();
    let path = dir.join("kalshi-2026-09-13.ndjson.gz");
    let records: Vec<Record> = read_records(&path)
        .unwrap()
        .collect::<anyhow::Result<_>>()
        .unwrap();
    assert_eq!(records[0].control().unwrap(), None);
    assert_eq!(records[1].control().unwrap(), Some(control));
    assert_eq!(
        records[1].raw,
        r#"{"event":"disconnected","reason":"websocket idle timeout"}"#
    );
    // A line from a phase 2 recording, verbatim shape.
    let old: Record = serde_json::from_str(
        r#"{"received_at_ms":1789343120404,"sequence":7,"kind":"ping_base64","raw":""}"#,
    )
    .unwrap();
    assert_eq!(old.control().unwrap(), None);
    drop(recorder);
    fs::remove_dir_all(dir).unwrap();
}

/// Production `dump` session with a real websocket drop (idle timeout after a
/// process stall), reconnect, resubscribe, and fresh snapshots. The digest
/// fixture is the file that live run wrote; see tests/fixtures/README.md.
#[tokio::test]
async fn recorded_disconnect_session_replays_to_live_digest() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let live = fs::read_to_string(fixtures.join("phase3-disconnect-session.digest")).unwrap();
    let run = replay(
        &fixtures.join("phase3-disconnect-session.ndjson.gz"),
        max(None),
    )
    .await;
    assert_eq!(run.digest.render(), live);
    assert!(
        Expected::parse(&live)
            .unwrap()
            .check(&run.digest)
            .is_empty()
    );
    assert_eq!(run.parse_errors, 0);
    assert_eq!(run.gaps.entries()[0], "579 disconnected kalshi");
    assert_eq!(run.gaps.entries().len(), 9);
    assert!(
        run.store
            .books()
            .iter()
            .all(|book| book.state == BookState::Live)
    );
}

/// The venue's own timestamp survives recording and replay unchanged and stays
/// separate from local receipt time; phase 7 skew tracking needs both.
#[tokio::test]
async fn venue_timestamps_survive_recording_and_replay_separately_from_receipt() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/phase3-disconnect-session.ndjson.gz");
    // Expected per book event, read straight from the recorded raw bytes.
    let mut expected = Vec::new();
    for record in read_records(&path).unwrap() {
        let record = record.unwrap();
        if record.kind != "text" {
            continue;
        }
        let wire: serde_json::Value = serde_json::from_str(&record.raw).unwrap();
        match wire["type"].as_str() {
            Some("orderbook_snapshot") => {
                expected.push((wire["msg"]["ts_ms"].as_u64(), record.received_at_ms))
            }
            Some("orderbook_delta") => {
                let ts_ms = wire["msg"]["ts_ms"].as_u64().unwrap();
                let rfc3339 =
                    chrono::DateTime::parse_from_rfc3339(wire["msg"]["ts"].as_str().unwrap())
                        .unwrap();
                assert_eq!(rfc3339.timestamp_millis() as u64, ts_ms);
                expected.push((Some(ts_ms), record.received_at_ms));
            }
            _ => {}
        }
    }
    let run = replay(&path, max(None)).await;
    let actual: Vec<Option<u64>> = run
        .events
        .iter()
        .filter_map(|event| match event {
            FeedEvent::Snapshot { venue_ts_ms, .. } => Some(*venue_ts_ms),
            FeedEvent::Delta { venue_ts_ms, .. } => Some(Some(*venue_ts_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(actual.len(), 2_280);
    assert_eq!(
        actual,
        expected.iter().map(|(venue, _)| *venue).collect::<Vec<_>>()
    );
    // Kalshi snapshots carry no venue time; receipt time is not substituted.
    assert_eq!(actual.iter().filter(|venue| venue.is_none()).count(), 8);
    // Every delta's venue time differs from its receipt time (about 3.4 s here).
    assert!(
        expected
            .iter()
            .all(|(venue, receipt)| *venue != Some(*receipt))
    );
    // Books are stamped with receipt time, not venue time.
    let (last_venue, last_receipt) = *expected.last().unwrap();
    let stamped: Vec<u64> = run.store.books().iter().map(|b| b.updated_at_ms).collect();
    assert!(stamped.contains(&last_receipt));
    assert!(!stamped.contains(&last_venue.unwrap()));

    // Deliberate mutations: a delta with only the RFC3339 `ts`, and a snapshot
    // that does carry `ts_ms`, keep their venue values.
    let ticker = "KXBTCD-26SEP1417-T76999.99".to_owned();
    let mut parser = Parser::new(std::slice::from_ref(&ticker)).unwrap();
    let delta = read_records(&path)
        .unwrap()
        .map(Result::unwrap)
        .map(|r| serde_json::from_str::<serde_json::Value>(&r.raw).unwrap_or_default())
        .find(|v| v["type"] == "orderbook_delta" && v["msg"]["market_ticker"] == ticker.as_str())
        .unwrap();
    let mut ts_only = delta.clone();
    let venue = ts_only["msg"]["ts_ms"].as_u64().unwrap();
    ts_only["msg"].as_object_mut().unwrap().remove("ts_ms");
    assert!(matches!(
        parser.parse(&ts_only.to_string(), 42),
        Some(FeedEvent::Delta { venue_ts_ms, .. }) if venue_ts_ms == venue
    ));
    let snapshot = serde_json::json!({"type": "orderbook_snapshot", "sid": 1, "seq": 1,
        "msg": {"market_ticker": ticker, "yes_dollars_fp": [["0.4400", "10.00"]], "ts_ms": 7}});
    assert!(matches!(
        parser.parse(&snapshot.to_string(), 42),
        Some(FeedEvent::Snapshot {
            venue_ts_ms: Some(7),
            ..
        })
    ));
}

#[tokio::test]
async fn realtime_pace_sleeps_recorded_gaps_and_max_does_not() {
    let frames = fixture("stage2-live-orderbook.ndjson");
    let entries = vec![
        Entry::Text(BASE_MS, frames[1].clone()),
        Entry::Text(BASE_MS + 120, frames[2].clone()),
        Entry::Text(BASE_MS + 300, frames[3].clone()),
    ];
    let dir = temp();
    let path = record(&dir, &entries);
    let timed = |pace| {
        let path = path.clone();
        async move {
            let start = tokio::time::Instant::now();
            let run = replay(
                &path,
                ReplayOptions {
                    pace,
                    ..max(Some(tickers()))
                },
            )
            .await;
            (start.elapsed(), run.digest)
        }
    };
    let (realtime, realtime_digest) = timed(Pace::Realtime).await;
    let (flat_out, max_digest) = timed(Pace::Max).await;
    assert!(realtime.as_millis() >= 300, "{realtime:?}");
    assert!(flat_out.as_millis() < 300, "{flat_out:?}");
    assert_eq!(realtime_digest, max_digest);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cli_replay_verifies_with_no_credentials_in_environment() {
    let (entries, _) = resync_session(true);
    let dir = temp();
    let path = record(&dir, &entries);
    let digest = dir.join("expected.digest");
    let sum100 = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sum100"));
        // No KALSHI_KEY_ID, no KALSHI_PRIVATE_KEY_PATH, no HOME, no proxy.
        command.env_clear();
        command
    };
    let first = sum100()
        .args(["replay", "--venue", "kalshi", "--file"])
        .arg(&path)
        .arg("--digest-out")
        .arg(&digest)
        .output()
        .unwrap();
    assert!(first.status.success(), "{first:?}");
    let text = fs::read_to_string(&digest).unwrap();
    assert!(String::from_utf8(first.stdout).unwrap().ends_with(&text));
    assert!(text.contains(&format!("gap 8 resubscribed {TICKER}")));

    let verified = sum100()
        .args(["replay", "--venue", "kalshi", "--verify", "--expect-file"])
        .arg(&digest)
        .arg("--file")
        .arg(&path)
        .output()
        .unwrap();
    assert!(verified.status.success(), "{verified:?}");

    let expected = Expected::parse(&text).unwrap();
    let book = format!("{TICKER}={}", expected.books[TICKER]);
    let inline = sum100()
        .args([
            "replay",
            "--venue",
            "kalshi",
            "--verify",
            "--expect-book",
            &book,
        ])
        .args(["--expect-gaps", expected.gaps.as_deref().unwrap(), "--file"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(inline.status.success(), "{inline:?}");

    let wrong = sum100()
        .args([
            "replay",
            "--venue",
            "kalshi",
            "--verify",
            "--expect-book",
            &book,
        ])
        .args(["--expect-gaps", &"f".repeat(64), "--file"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("verify FAILED gaps"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn no_wall_clock_reads_outside_clock_module() {
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    assert!(files.len() > 10);
    for file in files {
        let relative = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&file).unwrap();
        if relative != "clock.rs" {
            for banned in ["Utc::now", "Local::now", "SystemTime", "UNIX_EPOCH"] {
                assert!(
                    !text.contains(banned),
                    "{relative} reads wall time via {banned}"
                );
            }
        }
        // Monotonic instants may only schedule live socket deadlines and
        // realtime pacing; they never produce a timestamp.
        if !["feed/kalshi.rs", "feed/replay.rs"].contains(&relative.as_str()) {
            assert!(
                !text.contains("Instant::now"),
                "{relative} reads Instant::now"
            );
        }
    }
}
