# Phase 2.1 summary: one-sided snapshot fix and venue timestamp

Date: September 14, 2026 (UTC). Branch `phase-3-and-2.1`, not pushed or merged.

| Commit | Content |
|---|---|
| `712eb42` | Task 1: phase 3, committed as it stood (no edits) |
| `ebffead` | Task 3: one-sided snapshot fix, fixture, tests |
| `0f04b3e` | Task 4: `venue_ts_ms` on `FeedEvent` |
| `fd97b58` | Not part of this work: "chore: add Makefile shortcuts for the live CLI recipes", committed to this branch separately during the session. It touches only `Makefile`, `README.md`, and `ARCHITECTURE.md`; no code changed (`git diff --stat 0f04b3e fd97b58 -- src tests Cargo.toml Cargo.lock` is empty) |
| docs commit | Task 2 (`docs/phase-2-summary.md`) and this summary with evidence |

Evidence: [`docs/phase-2-1-evidence/`](phase-2-1-evidence/). **"Before"** is the release
binary built from `712eb42` in a detached worktree; **"after"** is the release binary
built from `0f04b3e`. Replays ran under `sandbox-exec -p '(version 1)(allow
default)(deny network*)' env -i NO_COLOR=1`.

## 1. Exit criteria

| # | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | Phase 3 committed as its own commit before any task 3 or 4 edit | **Met** | §2.1 |
| 2 | `cargo test`, `clippy -D warnings`, `fmt --check` clean | **Met**: 44 tests | §2.2 |
| 3 | `data/phase3-d` replays with zero false gaps and zero forced reconnects, versus 227 | **Met**: 227 gaps / 227 resync requests → 0 / 0. The 227 recorded disconnect envelopes still replay as recorded history (§2.3); same 80 tickers live: 0 reconnects (§2.5) | §2.3, §2.5 |
| 4 | Deltas applied to far strikes that previously never went live | **Met**: D replay 51 never-live books → all live, 1,386 deltas on 12 of them; live E: all 53 one-sided books got deltas | §2.4, §2.5 |
| 5 | Malformed snapshot still rejected and counted, in a distinct test | **Met** | §2.6 |
| 6 | Phase 3 verifications pass, or each changed digest is named and justified | **Met**: all pass except session D, which changed as the fix requires | §2.7 |
| 7 | Venue timestamp on `FeedEvent` and in newly recorded files | **Met**: field is on the event; in new files it lives in the raw payload, see §2.8 for why | §2.8 |
| 8 | `docs/phase-2-summary.md` exists and states the open thirty-minute gate | **Met** | §2.9 |

## 2. Evidence

### 2.1 Commit order

```
$ git log --oneline 08ce95e..HEAD   (before the docs commit)
fd97b58 chore: add Makefile shortcuts for the live CLI recipes   (separate commit, not this work)
0f04b3e feat: carry the venue timestamp on FeedEvent separately from receipt time
ebffead fix: apply one-sided Kalshi snapshots instead of rejecting them
712eb42 feat: deterministic replay with injected clock and control envelopes
```

Before `712eb42`, `git status` showed exactly the phase 3 tree: 13 modified tracked files,
plus new `docs/`, `src/clock.rs`, `src/feed/replay.rs`, `src/verify.rs`,
`tests/replay.rs`, and the disconnect fixture. The gate re-ran clean (40 tests) and the
commit used `git add -A` with no edits. The tree was clean afterwards, before any task 3
edit.

### 2.2 Gate

[`phase-2-1-evidence/gate.txt`](phase-2-1-evidence/gate.txt), at HEAD `0f04b3e`:

```
$ cargo fmt --all -- --check                  exit=0
$ cargo clippy --all-targets -- -D warnings   exit=0
$ cargo test --all                            exit=0
    unittests src/lib.rs      14 passed
    tests/book.rs              9 passed
    tests/replay.rs           10 passed   (+1: venue timestamps)
    tests/snapshot_sides.rs    3 passed   (new)
    tests/stage2.rs            8 passed
$ cargo build --release                       exit=0
$ git diff --stat 712eb42 0f04b3e -- Cargo.toml Cargo.lock    (empty: no new dependencies)
$ grep -rn "Utc::now\|Local::now\|SystemTime\|UNIX_EPOCH" src   → only src/clock.rs
```

### 2.3 Session D before and after

[`phase-2-1-evidence/session-d/`](phase-2-1-evidence/session-d/). Both runs verify against the
live digest phase 3 recorded for session D.

```
$ sandbox-exec ... env -i NO_COLOR=1 <binary> replay --venue kalshi \
    --file data/phase3-d/production/kalshi-2026-09-14.ndjson.gz \
    --verify --expect-file docs/phase-3-evidence/session-d/live.digest(.gz, decompressed)
```

| | Before (`712eb42`) | After (`0f04b3e`) |
|---|---|---|
| verify against the live D digest | `verify OK: 80 book hash(es) and gap log hash match` (reproduces the live storm) | `verify FAILED gaps …` and 55 book lines: expected, see §2.7 |
| `parse_errors` | 11,654 | **0** |
| `snapshot_sides_absent` | (metric did not exist) | 11,654 |
| `snapshots_applied` | 6,506 | 18,160 |
| `deltas_applied` | 0 | **2,192** (every delta in the file) |
| `deltas_skipped_not_live` | 1,965 | 0 |
| `sequence_gaps` | **227** | **0** |
| `resync_requests` (each one a forced reconnect when live) | **227** | **0** |
| gap log entries | 227 `gap`, 227 `disconnected`, 18,080 `resubscribed`, 6,477 `resynced` | 0 `gap`, 227 `disconnected`, 18,080 `resubscribed`, 18,080 `resynced` |

**What "zero forced reconnects" means for this recording.** The live run *did* reconnect
227 times, and each one is a recorded control envelope. Replay reproduces recorded transport
history rather than inventing a different one, so 227 `disconnected` entries still appear.
What the fix removes is the cause: replay now raises no gap and no resync request, so a
live feed on the fixed code would have had nothing forcing those reconnects. Session E (§2.5)
shows that directly on production.

### 2.4 Far strikes in session D

[`session-d/per-contract.txt`](phase-2-1-evidence/session-d/per-contract.txt) counts
`applied=Snapshot(..)` and `applied=Delta(..)` per contract from `RUST_LOG=debug` replays.

```
tickers 80 | ever one-sided in D 54 | two-sided only 26
contracts with 0 snapshots applied before fix: 51 (all one-sided: True)
of those, snapshots applied after fix > 0: 51
of those, deltas applied after fix > 0: 12 | deltas applied to them before: 0 after: 1386
deltas applied total before 0 after 2192
```

For example: `T81749.99` 0 → 599 deltas, `T82249.99` 0 → 407, `T81999.99` 0 → 352,
`T73749.99` 0 → 10. The other 39 never-live books had no delta traffic anywhere in the
recording, so none existed to apply.

### 2.5 Live production check after the fix (session E)

[`phase-2-1-evidence/session-e/`](phase-2-1-evidence/session-e/): the same 80 tickers as
session D, the fixed binary, 180 s. A guard would have sent SIGINT after 3 disconnects.
Script: [`scripts/session_e.sh`](phase-2-1-evidence/scripts/session_e.sh).

```
$ ./target/release/sum100 dump --venue kalshi --prod --tickers <80 KXBTCD-26SEP1417 strikes> \
    --out data/phase2-1-e --seconds 180 --interval-ms 600000 \
    --digest-out docs/phase-2-1-evidence/session-e/live.digest
start 2026-09-14T04:13:25Z binary 0f04b3e
exit 0 2026-09-14T04:16:26Z                    (guard did not fire)
grep -c 'feed disconnected' live.stderr → 0
... dump finished ... Metrics { messages_received: 14461, parse_errors: 0, reconnections: 0,
    snapshot_sides_absent: 53, ... }
BookMetrics { snapshots_applied: 80, deltas_applied: 14380, deltas_skipped_not_live: 0,
    sequence_gaps: 0, resync_requests: 0, ... } uninitialized=0 resyncing=0 live=80
```

| Same 80 tickers, production | Session D (phase 3, pre-fix) | Session E (fixed) |
|---|---|---|
| Reconnects | 227 in 7 min | **0** in 180 s |
| Books live at end | 0 | **80** |
| Deltas applied | 0 | **14,380** |

All 53 books that were one-sided at snapshot had deltas applied (1,094 in total, at least
2 each): [`session-e/venue-ts-and-far-strikes.txt`](phase-2-1-evidence/session-e/venue-ts-and-far-strikes.txt).
The replay of E verifies against E's own live digest (`verify OK: 80 book hash(es) and
gap log hash match`, `diff` empty): [`session-e/replay-max.txt`](phase-2-1-evidence/session-e/replay-max.txt).
Recording SHA-256 `0edf5a850b0f1c4742d7ae34320a4025371ce5064d8b361facc8b19c537e55b7`.

### 2.6 Tests (`tests/snapshot_sides.rs`)

Fixture `tests/fixtures/phase3-d-one-sided-snapshots.ndjson.gz` (11,836 bytes, SHA-256
`0501b383…aee7c`) holds 155 envelope lines copied byte for byte from session D:
- the session marker;
- one connection's ack, 80 snapshots, and 73 deltas at contiguous venue seq 1–153;
- 31 snapshots without `no_dollars_fp` and 20 without `yes_dollars_fp`;
- 70 deltas on those strikes.

Live, this connection ended in a false gap and a forced reconnect. Provenance is in
`tests/fixtures/README.md`.

| Binary on the fixture | parse errors | snapshots | deltas | gaps | resync requests |
|---|---|---|---|---|---|
| before `712eb42` | 51 | 29 | 0 (72 skipped) | 1 (`expected=61 got=150`) | 1 |
| after `0f04b3e` | 0 | 80 | 73 | 0 | 0 |

- **`one_sided_snapshots_from_session_d_stay_live_without_false_gaps`**
  - After each snapshot: the book is `Live` at that seq and the expected sequence is `seq + 1`.
  - At that moment the absent side reads empty on all 101 prices, its best level is `None` (and `asks()` is empty when `no` is absent), and the present side is non-empty.
  - Every delta arrives at `last + 1`.
  - Totals: 0 parse errors, 51 absent sides, expected seq 154, 80 snapshots, 73 deltas (70 on far strikes).
  - Zero `sequence_gaps`, zero `resync_requests`, zero skipped deltas, an empty gap log, and all 80 books `Live`.
- **`malformed_snapshots_are_still_rejected_and_counted`** is separate. A control check confirms the captured one-sided payload is accepted. Then 8 deliberate mutations of it are each rejected, `parse_errors` rising by exactly 1 per mutation and `snapshot_sides_absent` never counting them:
  1. side is a string
  2. side is an object
  3. row is not a pair
  4. row values are numbers
  5. sub-cent price
  6. both side keys absent
  7. missing `market_ticker`
  8. missing `seq`
- **`absent_side_clears_stale_levels_and_null_is_not_counted_absent`**: a stale two-sided book (no 95x40) followed by a disconnect and the captured one-sided snapshot leaves the no side all zeros, so no stale level survives. Explicit `null` still parses as empty and is not counted as absent.

### 2.7 Phase 3 digest comparisons with the fixed binary

[`phase-2-1-evidence/phase3-reverify/results.txt`](phase-2-1-evidence/phase3-reverify/results.txt)

| Recording | Expected | Result |
|---|---|---|
| Session A | `docs/phase-3-evidence/session-a/live.digest` | `verify OK: 4 …`, `diff` identical |
| Session B | `…/session-b/live.digest` | `verify OK: 4 …`, `diff` identical |
| Session C (also the `phase3-disconnect-session` fixture test) | `…/session-c/live.digest` | `verify OK: 4 …`, `diff` identical |
| stage2-validation | book `a7a99901…4b4f89`, gaps `e3b0c442…b855` | `verify OK` |
| stage2-final-validation | book `b91985ca…c5f596`, gaps `e3b0…b855` | `verify OK` |
| production-2026-09-14 (placeholder + T76999.99) | `f10022ff…682e92f`, `8203ff84…c51e6af77`, gaps `e3b0…b855` | `verify OK`. The placeholder's snapshot, which has neither side, is still rejected, now reported as `snapshot has neither side` |
| **Session D** | `…/session-d/live.digest` | **Changed**: gap log `a853803b…` → `be853c97…`, and 55 of 80 book hashes |

**Why session D changed:** [`session-d/changed-digests.txt`](phase-2-1-evidence/session-d/changed-digests.txt).
- **52 books:** they were one-sided at least once, and those snapshots now apply.
- **3 always-two-sided books** (`T77249.99`, `T77999.99`, `T78249.99`): D's final connection carried exactly 4 deltas, all on these. Before the fix they were skipped after the false gap; now they apply.
- **25 unchanged books** are two-sided with no deltas in that connection.
- **2 mixed-shape books** (`T80749.99`, `T80999.99`) are unchanged because they end two-sided with no final deltas.
- **Gap log:** it loses its 227 `gap` entries and gains `resynced` entries for snapshots that now apply.
- **Sessions A, B, C:** they contain no one-sided snapshot (`snapshot_sides_absent: 0`), which is why they are unchanged.

Task 4 changes no digest: digests exclude timestamps, and `Book.updated_at_ms` still comes
from the clock.

### 2.8 Venue timestamp

`FeedEvent::Delta { venue_ts_ms: u64 }` and `FeedEvent::Snapshot { venue_ts_ms: Option<u64> }`
replace `ts_ms`.
- **Deltas:** the value comes from `ts_ms`, or else from the RFC3339 `ts`; a delta with neither is still a parse error.
- **Snapshots:** it is the venue's `ts_ms` when present. Kalshi sends none: across all 8 recordings, 0 of 18,260 snapshots had `ts_ms` or `ts`.
- **What was fixed:** previously the snapshot field silently became local receipt time.
- **Receipt time** stays in the injected clock and the envelope's `received_at_ms`.
- **Readers:** none yet.

**In newly recorded files.** The recorder writes the venue payload before parsing
(ARCHITECTURE §4.7), so the venue timestamp is always in the file, inside `raw`. It was never
lost from phases 1–3 recordings. I did not add a parsed `venue_ts_ms` to the envelope,
because that would make the recorder parse and break its write-before-parse invariant. Proof
on the file session E recorded after the fix, replayed with `RUST_LOG=debug`:

```
book events on FeedEvent: 14460 | book payloads in file: 14460
kinds match in order: True
FeedEvent venue_ts_ms == raw msg.ts_ms for every event: True
snapshots: venue_ts_ms None: 80 of 80
deltas: 14380 | venue_ts_ms equal to received_at_ms: 0 | received - venue ms min/p50/max: 3419 3426 5982
```

The test `venue_timestamps_survive_recording_and_replay_separately_from_receipt` checks the
same on the checked-in phase 3 recording:
- all 2,280 events match raw `ts_ms`;
- RFC3339 `ts` equals `ts_ms` for every delta;
- 8 snapshots are `None`;
- books are stamped with receipt time, not venue time;
- a `ts`-only delta and a snapshot that carries `ts_ms` keep their venue values.

The phase 2 test that asserted snapshot `ts_ms == 1234` (the receipt argument) now asserts
`venue_ts_ms == None`.

### 2.9 Phase 2 summary

[`docs/phase-2-summary.md`](phase-2-summary.md) labels every claim as re-run at `08ce95e`
(29 tests pass today), replay-derived (the three pinned phase 3 digests), or the operator's
account with no artifact (the UI spot check). It carries the thirty-minute side-by-side gate
as **OPEN** with a waiver, and lists what the spot check did not cover.

## 3. Decisions and deviations

- **Both side keys absent is still rejected.** The prompt's rule, read literally, would
  treat it as two empty sides. I kept it an error because a single present side proves the
  payload is the expected schema, while both keys absent is byte-identical to Kalshi
  renaming the side keys. That would silently mark every book `Live` and empty, then build
  wrong books from deltas. It is also the only both-absent shape ever observed, and only for
  a nonexistent market (`YOUR-OPEN-TICKER`, empty `market_id`). Genuinely empty markets
  have never been seen. **The cost:** if a real market with no resting orders arrives, it
  hits the residual risk below. Accepting both-absent is a one-line change in
  `src/feed/kalshi.rs` if you prefer the literal rule.
- **No book store change.** Once a one-sided snapshot parses, `BookStore::apply_snapshot`
  already rebases the expected sequence and `Book::apply_snapshot` already rebuilds both
  arrays, so the absent side reads zero. The defect was entirely the parser rejecting valid
  messages. `src/book.rs` changed only to rename the ignored field (`ts_ms: _` → `venue_ts_ms: _`).
- **New metric** `Metrics::snapshot_sides_absent`, so absent-side handling is visible rather
  than silent.
- **The regression fixture is a slice**, not the whole 2.5 MB D recording: record 1 plus
  one connection, with that connection's three reconnect-related control envelopes omitted
  so it replays as a fresh session. The lines are verbatim; the method is in
  `tests/fixtures/README.md`.
- **Scope.** Beyond tasks 3 and 4, the only additions are
  - the session E live run;
  - short doc updates: README feed-boundary paragraph, ARCHITECTURE §4.1 event shape and snapshot-side sentence, `tests/fixtures/README.md`, and one PLAN.md checkpoint line.
  
  Nothing under solver, fees, Polymarket, registry, or frontend was touched. `git diff --stat 712eb42 0f04b3e` covers `src/feed/{kalshi,mod}.rs`, `src/metrics.rs`, `src/book.rs` (rename), tests, fixtures, and docs.
- **New dependencies:** none.

## 4. Waivers and residual risks

- **No criterion is waived.** Criterion 3 carries the clarification in §2.3: replay still
  shows D's 227 recorded disconnects as history. The gaps and resync requests that caused them are
  zero, and the production check (session E) shows 0 reconnects on the same ticker set.
- **Residual risk: a genuinely malformed sequenced message can still loop.** Any rejected
  message that carries a subscription seq, including a both-absent snapshot, still leaves
  the store's expected sequence behind. The next delta raises a gap and `dump` forces a
  reconnect. If the venue resends the same bad message, the loop recurs, and backoff resets
  to attempt 0 on each acknowledgement. The fix removes the one known production trigger,
  but not the mechanism. Closing it properly means the parser telling the store which seq a
  rejected message consumed, or rate-limiting repeated identical gaps. That changes the
  `FeedEvent` boundary, so I left it for an explicit decision.
- **Carried from phase 2:** the thirty-minute side-by-side UI gate remains open (see
  `docs/phase-2-summary.md` §5). Session E is a determinism and stability check, not a UI
  comparison.
- **Worktrees.** The `712eb42` and `08ce95e` worktrees used for before-and-after binaries
  lived in the session scratchpad and were removed after the evidence was collected.
