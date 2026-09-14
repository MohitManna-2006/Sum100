# Phase 2 summary: book store (retroactive)

Written September 14, 2026, after phase 3. No summary was written when phase 2 shipped.
Phase 2 is commit `08ce95e` ("feat: add book store with live dump and sequence gap
resync", 2026-09-13 22:55:53 −04:00).

Every claim below carries one of three labels:

- **[re-run]**: executed again today against `08ce95e` itself.
- **[replay]**: computed by phase 3's replay tooling from a recording made in the phase 2 period. Phase 2 had no replay or digest capability.
- **[account]**: the operator's report, with no artifact in the repository.

Evidence files are in [`docs/phase-2-evidence/`](phase-2-evidence/).

## 1. Exit criteria (PLAN.md, phase 2)

| Criterion | Status | Evidence |
|---|---|---|
| Snapshot application | Done | §2.1 [re-run] |
| Delta application with level insert, update, remove | Done | §2.1 [re-run] |
| Sequence tracking and gap detection | Done offline; not exercised live in phase 2 | §2.1 [re-run], §4 |
| `Resyncing` state and snapshot re-request | Done offline; first live exercise was in phase 3, where it looped | §2.1 [re-run], §4 |
| Metrics for applied messages, gaps, books by state | Done (`BookMetrics`, `books_by_state`) | code at `08ce95e` |
| `dump` subcommand showing best bid and ask | Done | code at `08ce95e`; used live (§2.3) |
| **Done when:** thirty minutes side by side with the Kalshi web UI shows no divergence, and any divergence was preceded by a logged gap | **OPEN, carried as a waiver** | §2.3, §5 |

## 2. Evidence

### 2.1 Tests at `08ce95e` [re-run]

```
$ git worktree add --detach <scratch>/wt-08ce95e 08ce95e
$ cd <scratch>/wt-08ce95e && cargo test --all
    unittests src/lib.rs   12 passed   (price parser, fee schedule, solver examples)
    tests/book.rs           9 passed
    tests/stage2.rs         8 passed
exit=0
```

Full output: [`phase-2-evidence/tests-at-08ce95e.txt`](phase-2-evidence/tests-at-08ce95e.txt).
The phase 2 book tests:

| Test | What it pins |
|---|---|
| `full_fixture_replay_reaches_pinned_final_book` | All 461 events of the captured stage 2 session: seq 461, expected seq 462, best yes bid 44x4294, best yes ask 46x100, best no bid 54x100, 26 bid and 26 ask levels, 0 gaps, 0 clamps, 1 snapshot, 460 deltas |
| `no_side_delta_becomes_yes_ask_at_complement` | Real no-side delta (no 55 −1000) becomes yes ask 45 at 1105 |
| `sequence_gap_marks_resyncing_and_preserves_contents` | Skipped seq 3 gives `Gap { expected: 3, got: 4 }`, `Resyncing`, levels unchanged |
| `deltas_dropped_while_resyncing_until_snapshot` | Deltas skipped until a snapshot rebases the sequence |
| `disconnect_invalidates_and_resubscribe_awaits_snapshot` | `Disconnected` and `Resubscribed` require a fresh snapshot |
| `level_insert_update_and_remove` | 0 → 100 → 150 → 0 at one price |
| `floor_drift_clamps_at_zero` | Negative level clamps to 0, is counted, and the book stays `Live` |
| `crossed_book_stays_live_and_is_counted` | yes 60 + no 50 counted as crossed and stays `Live` |
| `parser_and_bookstore_agree_on_contract_ids` | Parser and store intern tickers identically |

All of these inputs are captured fixtures or synthetic events. None is a live phase 2 run.

### 2.2 Digests of the phase 2 period recordings [replay]

Phase 3 replayed these files and pinned their digests. Book hashes cover state, seq,
and every nonzero yes and no level; timestamps are excluded. Phase 3 changed the book
store only to take `updated_at_ms` from an injected clock, and that field is not hashed.
So these digests describe phase 2's book logic applied to these bytes. They were
re-verified with the phase 2.1 binary `0f04b3e`
([`phase-2-1-evidence/phase3-reverify/results.txt`](phase-2-1-evidence/phase3-reverify/results.txt)).

| Recording | What it is | SHA-256 | Pinned digest | Final book (replay) |
|---|---|---|---|---|
| `data/stage2-validation/production/kalshi-2026-09-13.ndjson.gz` | Phase 1 stage 2 recorder capture (70 s, source of the `stage2-live-orderbook` fixture), used as phase 2's offline input | `cc644cf5c8d34c31205f8ca2d714b5de3f1e411d32eb5fe03ff5ef35aacc5500` | `book KXBTCD-26SEP1417-T76999.99 a7a999017e785ef5c2f192665d6e73d4f0bbad0408d1a79c41d3e00ba45b4f89`; `gaps e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` (empty) | live, seq 461, 44x4294 / 46x100, the same book the phase 2 test pins |
| `data/stage2-final-validation/production/kalshi-2026-09-13.ndjson.gz` | Phase 1 stage 2 recorder capture (65 s) | `4f42d5da3a126c07fa0b871173e4d5de7ad699127941a9bf0e531554be8ecd8a` | `book KXBTCD-26SEP1417-T76999.99 b91985ca94d968731bdd60d22bc1c5b6883eba1b92df5c0b9187dfdc6a18f596`; `gaps e3b0…b855` (empty) | live, 276 events, 0 gaps |
| `data/production/kalshi-2026-09-14.ndjson.gz` | The only `dump` recording from the phase 2 period: a 50 s run with the README placeholder `YOUR-OPEN-TICKER` (02:43:03–02:43:53 UTC), then `KXBTCD-26SEP1417-T76999.99` from 02:44:16.987 to 02:49:16.667 UTC | `ae55be58e9106b20fe493debe927e50362d69ebf733ed206463a57f07852bcb2` | `book YOUR-OPEN-TICKER f10022fff2bf83d478d4a88545695ff95056d9fce23455a067a727b45682e92f`; `book KXBTCD-26SEP1417-T76999.99 8203ff840b8cafae9872422da29ab0a787a5816b092ba312a534e72c51e6af77`; `gaps e3b0…b855` (empty) | T76999.99 live, seq 1577, yes bid 66x8134, yes ask 67x3801, 1,576 deltas, 0 gaps; placeholder uninitialized (its snapshot has no sides and is rejected) |

The two runs in the last file carry no session markers, since they predate phase 3, so
replay treats the file as one session. Commands and output:
[`phase-3-evidence/legacy/`](phase-3-evidence/legacy/).

### 2.3 Live check against the Kalshi UI [account]

- **Reported:** during phase 2, `dump` was run live on `KXBTCD-26SEP1417-T76999.99` beside the Kalshi web UI, and best prices were spot-checked as matching.
- **In the repository:** no notes, screenshots, printed tables, or duration record of that comparison. The file in the last row of §2.2 is the only phase 2 period `dump` recording. Its timing (5 minutes of `T76999.99`, ending 6 minutes before the commit) fits the spot check, but nothing records that it was the session compared. Its final replayed book (seq 1577, 66x8134 / 67x3801) was never compared with the UI.
- **Not checked:** a thirty-minute comparison; more than one ticker; far or illiquid strikes; any live sequence gap or resync, since none occurred; any comparison at a recorded time against a recorded UI value.
- **At the time:** `PLAN.md` at `08ce95e` already said the thirty-minute check "remains an operator verification step via `dump`" ([`phase-2-evidence/plan-checkpoint-at-08ce95e.txt`](phase-2-evidence/plan-checkpoint-at-08ce95e.txt)).

## 3. What was built

- **Dense book** (`src/types.rs` `Book`): `[i64; 101]` resting size arrays for yes bids and no bids. Yes asks derive as `100 − no_price`; best-level queries scan the fixed arrays. Memory is bounded by construction.
- **Snapshot application** (`Book::apply_snapshot`): absolute; rebuilds both arrays, sets `Live`, rejects negative sizes and prices outside 0–100.
- **Delta application** (`Book::apply_delta`): one index update per signed delta. The floor-drift clamp holds a level at 0 when a floored fractional remove would go negative; it is counted in `negative_level_clamps` and the book stays `Live`, since understating depth is the safe direction.
- **Book store** (`src/book.rs` `BookStore`):
  - Sequence continuity is scoped to the subscription, not per contract, because Kalshi sequence numbers span tickers.
  - A delta applies only to a `Live` book at exactly the expected sequence.
  - A gap marks every live book `Resyncing`, leaves levels unchanged, clears the expectation, and returns `Applied::Gap`.
  - `Disconnected` and `Resubscribed` invalidate books until a fresh snapshot arrives.
- **Resync request:** on `Applied::Gap`, the `dump` driver calls `KalshiFeed::request_resync()`, which drops the socket so the existing reconnect path resubscribes and delivers fresh snapshots. Kalshi has no per-ticker snapshot request.
- **Crossed books:** best yes bid + best no bid > 100 stays `Live` and is counted in `crossed_books_observed`.
- **`dump` CLI:** a live best-bid/ask table on an interval, recording raw envelopes like `record`.

## 4. Defects found after phase 2

| Defect | Found | Fixed |
|---|---|---|
| Kalshi omits an empty snapshot side's key for far strikes. The parser rejected those snapshots, but they consumed sequence numbers, so the store saw a false gap and `dump` forced a reconnect that received the same snapshots: 227 production reconnects in 7 minutes, no deltas applied. Phase 2's only live ticker was near the money and always two-sided. | Phase 3, session D | Phase 2.1, `ebffead` |
| The resync path had never run live. Its first live exercise was that loop. | Phase 3, session D | Phase 2.1, `ebffead` (removes the false gaps); see phase 2.1 summary for residual risk |
| `FeedEvent::Snapshot.ts_ms` fell back to local receipt time when the venue sent none (every Kalshi snapshot), mixing receipt time into the venue timestamp. This dates from phase 1. | Phase 2.1 review | Phase 2.1, `0f04b3e` (`venue_ts_ms`) |
| `Book.updated_at_ms` used the venue timestamp, so freshness checks would have depended on venue clock skew (≈3.4 s observed). No live freshness check ran in phase 2; the solver was not wired to books. | Phase 3 | Phase 3, `712eb42` (injected clock) |
| `dump` shutdown could drop recorded but undelivered events, so its final state could differ from its own recording. | Phase 3 | Phase 3, `712eb42` |

## 5. Waivers

- **Thirty-minute side-by-side gate: OPEN, carried as a waiver.** It was never run or
  recorded. The reported spot check of one near-the-money ticker (§2.3) is partial evidence
  only: it covers none of the duration, no far strikes, and no gap or resync. Closing it
  needs a run of at least thirty minutes, recorded with `--digest-out`, with UI readings
  noted against wall-clock times, over tickers that include one-sided far strikes. Because
  of the defect in §4, this is meaningful only on phase 2.1 code or later.
- **Hour-long endurance session** (a phase 1 deliverable, still listed as pending in
  PLAN.md): not run in phase 2.
