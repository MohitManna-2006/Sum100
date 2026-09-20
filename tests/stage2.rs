use base64::Engine as _;
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pss::{Signature, VerifyingKey},
    signature::Verifier,
};
use sha2::Sha256;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use sum100::{
    feed::{
        FeedEvent,
        kalshi::{Parser, backoff_ms, sign},
    },
    record::{Recorder, read_records},
    types::{ContractId, Contracts, Side, Venue, parse_size_contracts},
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
fn temp() -> PathBuf {
    let path = std::env::temp_dir().join(format!("sum100-test-{}", rand::random::<u64>()));
    fs::create_dir_all(&path).unwrap();
    path
}
fn ticker() -> String {
    "KXBTCD-26SEP1417-T76999.99".into()
}

#[test]
fn captured_snapshot_and_every_consecutive_delta_parse() {
    let frames = fixture("stage1-orderbook.ndjson");
    let mut parser = Parser::new(&[ticker()]).unwrap();
    let mut seq = 0;
    let mut yes = 0;
    let mut no = 0;
    for raw in frames {
        match parser.parse(&raw, 1234) {
            Some(FeedEvent::Snapshot {
                contract,
                yes,
                no,
                seq: s,
                venue_ts_ms,
            }) => {
                assert_eq!(contract, ContractId(0));
                assert_eq!(s, 1);
                seq = s;
                // The snapshot carries no venue time; receipt time (1234) must not
                // be substituted into the venue field.
                assert_eq!(venue_ts_ms, None);
                assert_eq!((yes.len(), no.len()), (40, 48));
                assert_eq!((yes[0].price, yes[0].size), (1, 236781));
                assert_eq!(parser.metrics.discarded_size_hundredths, 479);
            }
            Some(FeedEvent::Delta {
                contract,
                side,
                price,
                size_delta,
                seq: s,
                venue_ts_ms,
            }) => {
                assert_eq!(contract, ContractId(0));
                assert_eq!(s, seq + 1);
                seq = s;
                let wire: serde_json::Value = serde_json::from_str(&raw).unwrap();
                assert_eq!(venue_ts_ms, wire["msg"]["ts_ms"].as_u64().unwrap());
                assert_eq!(
                    price,
                    sum100::types::parse_price_cents(
                        wire["msg"]["price_dollars"].as_str().unwrap()
                    )
                    .unwrap()
                );
                if s == 2 {
                    assert_eq!(
                        (side, price, size_delta, venue_ts_ms),
                        (Side::Yes, 34, -100, 1789342115869)
                    );
                }
                match side {
                    Side::Yes => yes += 1,
                    Side::No => no += 1,
                }
            }
            None => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((seq, yes, no), (157, 102, 54));
    assert_eq!(parser.metrics.parse_errors, 0);
}

#[test]
fn unknown_types_skip_and_malformed_frames_do_not_stop_parser() {
    let mut parser = Parser::new(&[ticker()]).unwrap();
    let ticker_frames = fixture("stage1-ticker.ndjson");
    for frame in ticker_frames {
        assert!(parser.parse(&frame, 0).is_none());
    }
    assert_eq!(parser.metrics.unknown_messages, 31);
    assert_eq!(parser.metrics.parse_errors, 0);
    assert!(parser.parse("{invalid", 0).is_none());
    assert_eq!(parser.metrics.parse_errors, 1);
    let frames = fixture("stage1-orderbook.ndjson");
    assert!(matches!(
        parser.parse(&frames[2], 0),
        Some(FeedEvent::Delta { .. })
    ));
    // Deliberate mutations test rejection, never masquerade as captured fixtures.
    let mut bad: serde_json::Value = serde_json::from_str(&frames[2]).unwrap();
    bad["msg"]["side"] = "bid".into();
    assert!(parser.parse(&bad.to_string(), 0).is_none());
    bad["msg"]["side"] = "yes".into();
    bad["msg"]["price_dollars"] = "0.005".into();
    assert!(parser.parse(&bad.to_string(), 0).is_none());
    assert_eq!(parser.metrics.parse_errors, 3);
}

#[test]
fn fixed_point_sizes_floor_with_exact_discard_counter() {
    let mut discarded = 0;
    for (s, want, loss) in [
        ("491.90", 491, 90),
        ("300.00", 300, 0),
        ("0.01", 0, 1),
        ("-1.20", -2, 80),
        ("-0.01", -1, 99),
        ("-54.00", -54, 0),
        ("1.2000", 1, 20),
        ("9223372036854775807", i64::MAX, 0),
        ("-9223372036854775808", i64::MIN, 0),
    ] {
        let before = discarded;
        assert_eq!(parse_size_contracts(s, &mut discarded).unwrap(), want);
        assert_eq!(discarded - before, loss);
    }
    for bad in [
        "",
        " ",
        "1.",
        ".2",
        "+1",
        "1e2",
        "NaN",
        "1.001",
        "--1",
        "9223372036854775808",
        "-9223372036854775808.01",
    ] {
        let before = discarded;
        assert!(parse_size_contracts(bad, &mut discarded).is_err(), "{bad}");
        assert_eq!(discarded, before);
    }
}

#[test]
fn rsa_pss_signature_verifies_documented_message_and_rejects_tampering() {
    let key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let signature = sign(&key, 1789342115869, "GET", "/trade-api/ws/v2?ignored=1").unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .unwrap();
    let signature = Signature::try_from(bytes.as_slice()).unwrap();
    let verifier = VerifyingKey::<Sha256>::new(RsaPublicKey::from(&key));
    verifier
        .verify(b"1789342115869GET/trade-api/ws/v2", &signature)
        .unwrap();
    assert!(
        verifier
            .verify(b"1789342115870GET/trade-api/ws/v2", &signature)
            .is_err()
    );
}

#[test]
fn recorder_round_trip_keeps_raw_bytes_without_blank_records() {
    let root = temp();
    let mut recorder = Recorder::new(&root, Venue::Kalshi).unwrap();
    let mut inputs = fixture("stage1-orderbook.ndjson");
    inputs.extend(fixture("stage2-live-orderbook.ndjson"));
    // Noncanonical JSON, invalid JSON, CR, spaces, and embedded newlines must
    // survive too; a parsed-JSON recorder would silently change or reject these.
    inputs.extend([
        "  { \"b\":1e2, \"a\":\"\\u0061\" } \r".into(),
        "not json".into(),
        "{\n\"a\": 1\n}".into(),
    ]);
    for (i, raw) in inputs.iter().enumerate() {
        recorder
            .write(1789342115869, "text", &format!("{raw}\n"))
            .unwrap();
        if i % 7 == 0 {
            recorder.flush().unwrap();
        }
    }
    recorder.flush().unwrap();
    let path = root.join("kalshi-2026-09-13.ndjson.gz");
    let records = read_records(&path)
        .unwrap()
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(records.len(), inputs.len());
    for (i, (record, input)) in records.iter().zip(&inputs).enumerate() {
        assert_eq!(record.raw.as_bytes(), input.as_bytes());
        assert_eq!(record.sequence, i as u64 + 1);
        assert_eq!(record.received_at_ms, 1789342115869);
    }
    let mut decoded = String::new();
    flate2::read::MultiGzDecoder::new(fs::File::open(&path).unwrap())
        .read_to_string(&mut decoded)
        .unwrap();
    assert_eq!(decoded.lines().count(), inputs.len());
    assert!(decoded.lines().all(|line| !line.is_empty()));
    assert!(!decoded.contains("\n\n"));
    drop(recorder);
    let mut resumed = Recorder::new(&root, Venue::Kalshi).unwrap();
    resumed.write(1789342115870, "text", "last").unwrap();
    resumed.flush().unwrap();
    assert_eq!(
        read_records(&path)
            .unwrap()
            .last()
            .unwrap()
            .unwrap()
            .sequence,
        inputs.len() as u64 + 1
    );
    resumed.write(1789430400000, "text", "next day").unwrap();
    resumed.flush().unwrap();
    assert!(root.join("kalshi-2026-09-15.ndjson.gz").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn interning_is_stable_and_venue_scoped_and_backoff_is_bounded() {
    let mut ids = Contracts::default();
    let a = ids.intern(Venue::Kalshi, "a").unwrap();
    assert_eq!(ids.intern(Venue::Kalshi, "a").unwrap(), a);
    assert_ne!(ids.intern(Venue::Polymarket, "a").unwrap(), a);
    assert_eq!(ids.resolve(a).unwrap(), &(Venue::Kalshi, "a".into()));
    assert_eq!(backoff_ms(0, 0), 250);
    for n in 0..100 {
        assert!((250..=30_000).contains(&backoff_ms(n, u64::MAX)));
    }
    assert_eq!(backoff_ms(100, 0), 30_000);
}

#[test]
fn fresh_live_session_and_real_no_side_price_are_preserved() {
    let mut parser = Parser::new(&[ticker()]).unwrap();
    let mut seq = 0;
    let mut deltas = 0;
    let mut no_deltas = 0;
    for raw in fixture("stage2-live-orderbook.ndjson") {
        match parser.parse(&raw, 1789343120404) {
            Some(FeedEvent::Snapshot {
                seq: s, yes, no, ..
            }) => {
                assert_eq!(s, 1);
                assert!(!yes.is_empty());
                assert!(!no.is_empty());
                seq = s;
            }
            Some(FeedEvent::Delta { seq: s, side, .. }) => {
                assert_eq!(s, seq + 1);
                seq = s;
                deltas += 1;
                if side == Side::No {
                    no_deltas += 1;
                }
            }
            None => {}
            event => panic!("unexpected {event:?}"),
        }
    }
    assert_eq!((deltas, no_deltas), (460, 209));
    assert_eq!(parser.metrics.parse_errors, 0);
    let no_frame = fixture("no-side-delta.json");
    assert_eq!(
        parser.parse(&no_frame[0], 0),
        Some(FeedEvent::Delta {
            contract: ContractId(0),
            side: Side::No,
            price: 55,
            size_delta: -1000,
            seq: 4,
            venue_ts_ms: 1789343119239,
        })
    );
    assert!(fixture("stage2-live-orderbook.ndjson").contains(&no_frame[0]));
}

#[test]
fn recorder_rejects_concurrent_writer_and_corrupt_append() {
    let root = temp();
    let mut recorder = Recorder::new(&root, Venue::Kalshi).unwrap();
    assert!(Recorder::new(&root, Venue::Kalshi).is_err());
    recorder
        .write(1789342115869, "text", "untouched\n\n")
        .unwrap();
    recorder.flush().unwrap();
    let path = root.join("kalshi-2026-09-13.ndjson.gz");
    assert_eq!(
        read_records(&path).unwrap().next().unwrap().unwrap().raw,
        "untouched\n"
    );
    drop(recorder);
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"broken gzip member")
        .unwrap();
    let before = fs::read(&path).unwrap();
    let mut recorder = Recorder::new(&root, Venue::Kalshi).unwrap();
    assert!(
        recorder
            .write(1789342115869, "text", "must not append")
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(recorder);
    fs::remove_dir_all(root).unwrap();
}
