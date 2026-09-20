# Phase 3 summary: replay

Date: September 14, 2026 (UTC). Baseline: `08ce95e` (phase 2 book store), plus the
uncommitted working tree described here. Nothing was committed or pushed.

Ground-truth note: the requested first input, `docs/phase-2-summary.md`, does not exist
in the tree or in git history. ARCHITECTURE.md (§4.1, §4.7, §6), PLAN.md (phase 3), and
the phase 2 source were used as ground truth instead.

## 1. Exit criteria

| #   | Criterion                                                                         | Status                                                                             | Evidence   |
| --- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | ---------- |
| 1   | `cargo test`, `cargo clippy`, `cargo fmt --check` clean                           | **Met**                                                                            | §2.1       |
| 2   | Max-pace replay book hashes identical to the live run                             | **Met**: 4 production sessions, 92 book hashes                                     | §2.2       |
| 3   | Replay gap log identical to the live gap log                                      | **Met**: same 4 sessions, up to 25,011 entries                                     | §2.2, §2.3 |
| 4   | A session with at least one real disconnect replays with the live resync behavior | **Met, with caveat**: the disconnect followed a stall induced with SIGSTOP; see §5 | §2.3       |
| 5   | Replay works with no credentials and no network                                   | **Met**: `env -i` inside a macOS sandbox that denies all network                   | §2.4       |
| 6   | An older file without control envelopes still replays                             | **Met**: 3 phase 2 recordings                                                      | §2.5       |

Raw logs, digests, and capture scripts are in `[docs/phase-3-evidence/](phase-3-evidence/)`.
Recordings under `data/` are gitignored; their SHA-256 values are below so they can be
matched. One recording, the session C disconnect session, is checked in as a test fixture.

## 2. Evidence

### 2.1 Gate

Full output: `[phase-3-evidence/gate.txt](phase-3-evidence/gate.txt)`. The commands match CI.

```
$ cargo fmt --all -- --check                  exit=0
$ cargo clippy --all-targets -- -D warnings   exit=0
$ cargo test --all                            exit=0
    unittests src/lib.rs   14 passed   (includes 2 new clock tests)
    tests/book.rs           9 passed
    tests/replay.rs         9 passed   (new)
    tests/stage2.rs         8 passed
$ cargo build --release                       exit=0
```

### 2.2 Live and replay hash comparison

Each live session ran `dump` against **production** with `--digest-out`. The live
process writes its digest when it finishes, before any replay of that file. Each
recording was then replayed at `--pace max` with no credentials and network
denied, verifying against the live digest file:

```sh
# live (docs/phase-3-evidence/scripts/session_{a,b,c,d}.sh)
./target/release/sum100 dump --venue kalshi --prod --tickers $T --out data/phase3-X \
  --seconds N --interval-ms 60000 --digest-out docs/phase-3-evidence/session-X/live.digest

# replay
sandbox-exec -p '(version 1)(allow default)(deny network*)' env -i NO_COLOR=1 \
  ./target/release/sum100 replay --venue kalshi --file data/phase3-X/production/kalshi-2026-09-14.ndjson.gz \
  --verify --expect-file docs/phase-3-evidence/session-X/live.digest \
  --digest-out docs/phase-3-evidence/session-X/replay-max.digest
diff docs/phase-3-evidence/session-X/live.digest docs/phase-3-evidence/session-X/replay-max.digest
```

`$T` is `KXBTCD-26SEP1417-T76999.99,-T77499.99,-T77749.99,-T77999.99` (full tickers in
the scripts). Session D used all 80 active strikes of `KXBTCD-26SEP1417`.

| Session                             | Live window (UTC)    | Recording SHA-256                                                  | Envelopes (text / control) | Events | Result                                                            |
| ----------------------------------- | -------------------- | ------------------------------------------------------------------ | -------------------------- | ------ | ----------------------------------------------------------------- |
| A: clean, 180 s                     | 03:14:59 to 03:17:59 | `3f1767916cc1eaab2906a3d3fa2cb3a543f93ffe661cbf557e98559625d5a3a6` | 3,890 / 1                  | 3,889  | `verify OK: 4 book hash(es) and gap log hash match`; `diff` empty |
| B: 100 s SIGSTOP, no drop           | 03:15:00 to 03:20:30 | `b41446dfbe2e9a557e6bf9a2359d7a42011a414254bc8a569aa98117c8199b5e` | 6,645 / 1                  | 6,644  | `verify OK: 4 …`; `diff` empty                                    |
| C: 480 s SIGSTOP, **disconnect**    | 03:19:21 to 03:29:21 | `7d092c75fc4dad8fd530e61d3c23bf1323c74268b4419dff824e554d936ee97e` | 2,282 / 4                  | 2,285  | `verify OK: 4 …`; `diff` empty                                    |
| D: 80 tickers, **227 resync drops** | 03:19:20 to 03:26:20 | `74cfa726635cdc1ce65cd0868b5372ad135129c717567033f89b87b08fad0d48` | 20,579 / 680               | 27,005 | `verify OK: 80 …`; `diff` empty                                   |

`diff` compares the whole digest file, so the informational metrics line also matched:
snapshots, deltas applied or skipped, gaps, resync requests, clamps, and the event count.
Parser counters logged at the end also matched live in every session: `messages_received`,
`parse_errors`, `discarded_size_hundredths`, and the latency buckets.

Hashes compared, as printed by both live and replay:

```
# Session A
book KXBTCD-26SEP1417-T76999.99 c20d1d24b54ffd9071c19cb7413c63cfbe000b0f5b144f95f60c393d4a597262
book KXBTCD-26SEP1417-T77499.99 bd6db36d7d6b8ec7496328e6d035d405c5a5b74ed997694ce49041f737d6f071
book KXBTCD-26SEP1417-T77749.99 58744e6ebc3e2f056a0b8cb66cefbb6d4681fc98159aa3e13acde710fcbd39b5
book KXBTCD-26SEP1417-T77999.99 5b23939eef0583537bc6b8d2b130a8ca92fce6ffa1e73d6dacca1ed18f6b635d
gaps e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855   (empty log)

# Session B
book KXBTCD-26SEP1417-T76999.99 d2d34ccb6e4cc9baf031eb4f125639feac6b77f1872480a4c8a7c62bfc45f1f7
book KXBTCD-26SEP1417-T77499.99 9556f59aa42d7780a5a7fca9249b7b9d7de5564442aba4abef8e493d69dd46cf
book KXBTCD-26SEP1417-T77749.99 c40ca651ea34cbbf8b742560f680d2a201d1e377d9f2c0312a3669d68388faf5
book KXBTCD-26SEP1417-T77999.99 ff53fb00e70f2803c1c3fe28fc3ed7ee6b53d97c11c2e0b90749360dd451ce67
gaps e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855   (empty log)

# Session C
book KXBTCD-26SEP1417-T76999.99 cb60591020797950352f45f099156cabe5fa59989713f071cd274ec2129b50d0
book KXBTCD-26SEP1417-T77499.99 5a3cc908bb8bc6a250d9ea0db297c315ee32f585b74e1dcdfc6053c0a60eb5ae
book KXBTCD-26SEP1417-T77749.99 8ad35ae609c7d8c708140b9c5fe1321b66c170cdecb5c2e9d3b5ba784a73e4ea
book KXBTCD-26SEP1417-T77999.99 ed2fc396a2c5fe4e88c8a77021a1f892c3d1e77a83a365c3f05eb117ea95f984
gaps e0644203f26097982ff8e24b4cf746860c0ec02ff2593f76c2c73dd77acd35f9   (9 entries)

# Session D (80 book lines; full files gzipped in session-d/)
gaps a853803b9a454cfb2b4ac32272133fc936ce75e1472362d174dc34456ed7cefc   (25,011 entries)
sha256(live.digest)       = f134a803865ddb7430285d00f30f1c751cec4dd9c910f4cdb6e79cc30202fbe3
sha256(replay-max.digest) = f134a803865ddb7430285d00f30f1c751cec4dd9c910f4cdb6e79cc30202fbe3
```

Session D's replay also reproduced the 227 live `sequence gap`
warnings in the same order (`cmp` of the `expected=… got=…` sequences, recorded in
`session-d/replay-max.txt`). Replay wall time at max pace: A 0.03 s, C 0.03 s, D 1.03 s.

### 2.3 Disconnect case observed

**Session C: the websocket was dropped and recovered through the normal failure path.**
The process was suspended (SIGSTOP) from 03:20:01 to 03:28:01 UTC. On resume, the feed's
existing 45 s idle watchdog declared the socket dead. It emitted `Disconnected`, reconnected
with backoff attempt 0, the venue acknowledged the resubscription, and four fresh snapshots
brought every book back to `Live`. Live log (`session-c/live.stderr`):

```
03:28:01.474879Z WARN sum100::feed::kalshi: websocket idle timeout
03:28:01.476184Z WARN sum100::feed::kalshi: feed disconnected reason="websocket idle timeout"
... dump finished ... Metrics { messages_received: 2282, parse_errors: 0, ..., reconnections: 1, ... }
```

The recorded stream around it (`session-c/control-context.txt`):

```
sequence=580 received_at_ms=1789356001435 kind=text    orderbook_delta seq=578 KXBTCD-26SEP1417-T77749.99
sequence=581 received_at_ms=1789356481474 kind=control {"event":"disconnected","reason":"websocket idle timeout"}
sequence=582 received_at_ms=1789356481930 kind=control {"event":"reconnected","attempt":0}
sequence=583 received_at_ms=1789356481949 kind=text    subscribed
sequence=584 received_at_ms=1789356481949 kind=control {"event":"resubscribed","tickers":[...4 tickers...]}
sequence=585 received_at_ms=1789356481949 kind=text    orderbook_snapshot seq=1 KXBTCD-26SEP1417-T76999.99
```

Live gap log and replay gap log are identical, including event positions:

```
gap 579 disconnected kalshi
gap 580 resubscribed KXBTCD-26SEP1417-T76999.99
gap 581 resubscribed KXBTCD-26SEP1417-T77499.99
gap 582 resubscribed KXBTCD-26SEP1417-T77749.99
gap 583 resubscribed KXBTCD-26SEP1417-T77999.99
gap 584 resynced KXBTCD-26SEP1417-T76999.99 seq=1
gap 585 resynced KXBTCD-26SEP1417-T77499.99 seq=2
gap 586 resynced KXBTCD-26SEP1417-T77749.99 seq=3
gap 587 resynced KXBTCD-26SEP1417-T77999.99 seq=4
```

This recording and its live digest are now fixtures
(`tests/fixtures/phase3-disconnect-session.{ndjson.gz,digest}`). The test
`recorded_disconnect_session_replays_to_live_digest` requires replay to reproduce the live
digest byte for byte on every `cargo test`.

**Session D: 227 real sequence gaps, each followed by the forced-reconnect resync.**
Every gap dropped the production socket, reconnected, resubscribed 80 tickers, and fetched
fresh snapshots. There were also 3 handshake timeouts (`connection failed; retrying error=deadline has elapsed`). Control envelopes: 227 `disconnected`, 226 `reconnected`,
226 `resubscribed`, 1 `session_started`; live `reconnections: 226`. Replay reproduced the
full 25,011-entry gap log. The gaps are false positives caused by a phase 2 defect (§4).
That does not affect replay fidelity, but it means these drops came from the client's
resync logic, not from the venue.

**Session B: no disconnect.** A 100 s SIGSTOP did not break the connection. After resume
the venue delivered the backlog (`messages_received` 1,373 → 3,883 within 20 s) with no
sequence gap. It is kept as an extra stall-and-burst determinism case.

### 2.4 No credentials, no network

`[phase-3-evidence/legacy/no-network-no-credentials.txt](phase-3-evidence/legacy/no-network-no-credentials.txt)`:

```
$ sandbox-exec -p '(version 1)(allow default)(deny network*)' /usr/bin/curl ... https://external-api.kalshi.com/...
curl: (6) Could not resolve host: external-api.kalshi.com                                 exit=6
$ /usr/bin/curl ... (same URL, outside the sandbox)                                       200  exit=0
$ sandbox-exec -p '...(deny network*)' env -i NO_COLOR=1 ./target/release/sum100 markets --prod --series KXBTCD
    2: error resolving DNS                                                                exit=1
$ sandbox-exec -p '...(deny network*)' env -i NO_COLOR=1 ./target/release/sum100 replay --venue kalshi \
    --file data/stage2-validation/production/kalshi-2026-09-13.ndjson.gz --tickers KXBTCD-26SEP1417-T76999.99 \
    --verify --expect-book KXBTCD-26SEP1417-T76999.99=a7a999017e785ef5c2f192665d6e73d4f0bbad0408d1a79c41d3e00ba45b4f89 \
    --expect-gaps e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
verify OK: 1 book hash(es) and gap log hash match                                         exit=0
```

`env -i` clears every environment variable, including `KALSHI_KEY_ID`,
`KALSHI_PRIVATE_KEY_PATH`, and `HOME`. All §2.2 replays ran the same way. The
`cli_replay_verifies_with_no_credentials_in_environment` test runs the binary with
`env_clear()`.

That verify run uses hashes supplied **on the command line**. §2.2 uses the file form.
A mismatch exits nonzero with `verify FAILED gaps: expected …, got …` (tested).

### 2.5 Older recordings (no control envelopes)

All three phase 2 recordings replay. See `[phase-3-evidence/legacy/](phase-3-evidence/legacy/)`.

| File                                                                  | SHA-256                                                | Result                                                                                                                                                                                                                                                                                             |
| --------------------------------------------------------------------- | ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `data/stage2-validation/production/kalshi-2026-09-13.ndjson.gz`       | `cc644cf5…aacc5500` (matches tests/fixtures/README.md) | 0 control envelopes. Without `--tickers`: `Error: session 1 predates session markers; pass --tickers as recorded`. With it: seq 461, yes bid 44x4294, yes ask 46x100, 461 events, 0 parse errors, the same final book pinned by phase 2 in `tests/book.rs`                                         |
| `data/stage2-final-validation/production/kalshi-2026-09-13.ndjson.gz` | `4f42d5da…be8ecd8a`                                    | 276 events, 0 gaps                                                                                                                                                                                                                                                                                 |
| `data/production/kalshi-2026-09-14.ndjson.gz`                         | `ae55be58…7852bcb2`                                    | Two unmarked process runs (a `YOUR-OPEN-TICKER` placeholder run, then `T76999.99`). Replayed as one session with both tickers: 1,577 events, final `T76999.99` seq 1577. The placeholder's one-sided snapshot produced the same `snapshot missing side` parse error the phase 2 parser raises live |

### 2.6 Pacing

Session A at `--pace realtime` (`[session-a/replay-realtime.txt](phase-3-evidence/session-a/replay-realtime.txt)`):
`real 179.66` s, against 179,643 ms from the first event-bearing record (the first snapshot)
to the last record. Pacing anchors on the first yielded event; the session marker and ack
312 ms earlier yield nothing. The digest was identical to live (`verify OK`, `diff` empty).
At `--pace max` the same file took `real 0.03`. The `realtime_pace_sleeps_recorded_gaps_and_max_does_not`
test checks elapsed ≥ 300 ms for records 300 ms apart, max < 300 ms, and equal digests.

## 3. What was built

**Clock (**`src/clock.rs`**).** `trait Clock: Send + Sync { fn now_ms(&self) -> u64 }`.
`WallClock` is the only wall-clock read in the crate. `ReplayClock` is a shared
`Arc<AtomicU64>` advanced with `fetch_max`, so it never moves backwards.
`ReplayFeed` advances it to a record's `received_at_ms` before yielding that record's
events.

The clock is injected into:

- `BookStore::new(venue, tickers, clock)`: `updated_at_ms` now comes from the clock, not from the event's venue `ts_ms`.
- `KalshiFeed::start`: receipt time.
- `kalshi::connect`: the auth signing timestamp.
- `Rest::new`: Retry-After date arithmetic.
- The `dump` table age, and `src/bin/probe.rs`.

`FeedEvent::ts_ms` still carries the venue timestamp for the later skew metric. Receipt
minus venue `ts_ms` was a steady ≈3.47 s in the stage 2 recording (min 3,464 ms,
max 3,724 ms), which is why venue time must stay out of the replay clock. The test
`no_wall_clock_reads_outside_clock_module` scans `src/` for `Utc::now`, `Local::now`,
`SystemTime`, and `UNIX_EPOCH` outside `clock.rs`.

**Control envelopes (**`src/record.rs`**).** `kind: "control"` is reserved. `Recorder::write`
rejects it, and only `write_control` produces it. `raw` holds
`{"event": "session_started" | "disconnected" | "reconnected" | "resubscribed", ...}`.
Parse-time distinction is by envelope `kind`: venue text shaped like a control object stays
venue text (tested). The `Record` struct shape is unchanged, so old files and old readers
still parse. `KalshiFeed` writes:

- `disconnected`: immediately before each `Disconnected` send, on both emitting paths.
- `resubscribed`: immediately before the `Resubscribed` sends.
- `reconnected`: after a later handshake succeeds.
- `session_started`: once in `start`.

**ReplayFeed (**`src/feed/replay.rs`**).** It streams `read_records` (MultiGzDecoder) and
implements `Feed`. `text` records go to `kalshi::Parser::parse(raw, received_at_ms)`, the
same call the live worker makes. Binary, ping, pong, and close kinds count as received and
are not parsed, as in live. Unknown kinds are an error. Control envelopes yield
`Disconnected`, or `Resubscribed` for each recorded ticker; the others yield nothing.

`open` makes one streaming pass that lists sessions and proves the file decodes before
anything is yielded. A replay error ends the stream, and `finish()` returns it. The CLI
then fails before printing a digest.

**Pacing.** `Pace::Max` (default) or `Pace::Realtime`. Realtime uses `sleep_until(anchor + (received - first_received))`, so sleep overshoot does not accumulate.

**Verification (**`src/verify.rs`**, CLI** `replay --verify`**).**

- `GapLog::apply(&mut store, &event)` is the single apply step used by both `dump` and `replay`. It logs `gap`, `disconnected`, `resubscribed`, and `resynced` (a snapshot applied to a `Resyncing` book), keyed by 1-based event position.
- Per-contract hash: SHA-256 of `ticker=… state=… seq=… yes=p:s,… no=p:s,…` (nonzero levels of both wire sides).
- Gap hash: SHA-256 of the LF-terminated entries.
- No timestamps enter either hash (tested by shifting every receipt time by 1 h).
- Expectations come from `--expect-file` (a `--digest-out` file) or from `--expect-book TICKER=SHA256` and `--expect-gaps SHA256`. Every replayed contract and the gap log must be covered.

**Live shutdown fidelity (**`KalshiFeed::stop`**).** Previously, shutdown cancelled the worker
future. A frame could be recorded while its event was never delivered, and events queued in
the channel were dropped, so a live run's final state could differ from its own recording.
The worker now observes stop only inside its `select!` loops. Record-then-send is never
interrupted. `dump` calls `stop()`, drains `next()` until `None` while applying events, then
prints its digest. `shutdown()` drains before joining, so a full channel cannot deadlock it.

## 4. Findings outside phase 3 scope (not fixed)

Session D exposed two phase 2 defects on production data. Both are outside this phase
(`src/book.rs` refactors and parser changes were excluded) and are left for a follow-up.

1. **One-sided snapshots are rejected.** For deep in- and out-of-the-money strikes, Kalshi
   omits the empty side's key entirely: recorded snapshot keys are `['market_id',  'market_ticker', 'yes_dollars_fp']`. The phase 2 parser treats a missing key as a
   schema error (`snapshot missing side`). That was a deliberate choice, but live data
   contradicts it. Session D logged 11,654 such rejections.
2. **A rejected sequenced message causes a false gap and a reconnect storm.** The rejected
   snapshots consume subscription `seq` values (61–80 here), but the book store never sees
   them. The next accepted delta raises `expected=61 got=81`, the driver forces a resync,
   and the reconnect receives the same one-sided snapshots. Over 7 minutes: 227 production
   reconnects (about one every 0.6 s after the backlog, because each ack resets backoff to
   attempt 0), `deltas_applied=0`, and all 80 books ended `Resyncing`. Any market with
   an empty side will trigger this. Phase 2's live check used one near-the-money ticker,
   which never sends a one-sided snapshot. Until this is fixed, do not run
   `dump` on wide ticker sets.

## 5. Waivers, caveats, and deviations

- **Criterion 4 caveat.** Both disconnects were provoked, and each is labelled with its
  cause. Session C's drop followed an 8-minute SIGSTOP stall and was detected client-side by
  the 45 s idle watchdog. The recording cannot show whether the venue had also dropped the
  connection. Session D's drops were client-initiated resyncs after real (false-positive)
  gaps. Every reconnect, resubscribe, and snapshot in both sessions was a real production
  exchange, and replay matched live exactly. **No spontaneous, venue-initiated disconnect
  was observed** in about 30 minutes of recording. If the criterion requires one, carry
  it as a waiver until the pending hour-long endurance session captures one.
- **Missing ground-truth document.** `docs/phase-2-summary.md` does not exist (see the note at the top).
- `exit 145` **in the B, C, and D timelines** is zsh's `wait` reporting the earlier
  SIGSTOP state change (128 + 17). Each dump completed normally: `dump finished and gzip flushed` is logged, and the digest was written.
- **Monotonic instants.** `tokio::time::Instant::now()` remains in `feed/kalshi.rs`
  (existing socket deadlines) and in the realtime pacing anchor in `feed/replay.rs`. Both
  schedule sleeps and never produce a timestamp, event field, book field, or hashed value.
  The test above enforces that no other file uses them.
- **Scope additions, each with its reason:**
  - `session_started` control event: daily files are appended to by several processes, so replay needs session boundaries to reproduce one live run.
  - `src/verify.rs`: the digest and shared apply step that `--verify` needs.
  - `KalshiFeed::stop` and the `dump` drain: without them, criterion 2 cannot hold (§3).
  - `dump --digest-out`: the live side of the comparison.
  - Clock injection into `rest.rs` and `probe.rs`: task 1's no-global-time rule.
  - The disconnect-session fixture (66 KB).
  - Short status edits to README.md, ARCHITECTURE.md §4.1/§4.7, PLAN.md, and tests/fixtures/README.md.
- `src/book.rs` changed only to hold `Arc<dyn Clock>`, take it in `new`, and pass
  `clock.now_ms()` as the update time. `tests/book.rs` changed only to pass a clock.
- **New dependencies: none.** `Cargo.toml` and `Cargo.lock` are unchanged. Hashing uses
  the existing `sha2` crate.
- **Known limitations:**
  - A run that crosses UTC midnight is split across two daily files and cannot be replayed as one session.
  - An older file holding several unmarked runs replays as one concatenated session.
  - `GapLog` keeps every entry in memory, which is fine for verification runs but not a weeks-long `dump`.
  - `ReplayFeed` reads the gzip file synchronously inside `next()`.
  - Session D's large logs and digests are gzipped in `session-d/`. The commands in its `replay-max.txt` were run on the uncompressed files, and `digests.sha256` records both digest hashes before compression.
