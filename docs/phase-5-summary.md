# Phase 5 summary: registry loading and the engine task

Date: September 20, 2026 (UTC). Baseline: `b5a1c46`.

Scope: load the constraint graph from TOML, build the reverse index, validate it
(offline, and against live venue metadata), and wire the engine task that applies
book updates, marks groups dirty, calls the solver, and publishes state. The
paper executor (phase 6), the Polymarket feed (phase 7), and the HTTP API layer
stay out; the engine exposes the seams they plug into.

## 1. Exit criteria

| # | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | Registry loads from TOML and resolves all tickers | **Met, split in two**: loading is offline, metadata confirmation is `registry validate --live` | §2.2, §3 |
| 2 | Engine applies updates, marks dirty, calls the solver | **Met** | §2.3 |
| 3 | All dirty groups evaluated exactly once per update | **Met**: 4 updates × 2 groups = 8 evaluations, ladder never twice | §2.3 |
| 4 | Replay produces identical opportunity logs and state broadcasts | **Met**: byte-identical serialized streams across two runs | §2.4 |
| 5 | `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check` | **Met** | §2.1 |
| 6 | `scan --live` connects and receives market data | **Not verified**: needs credentials and an open market; the code path is wired and shares the feed the `dump` command already uses in production | §4 |
| 7 | `scan --replay <phase3-d file>` produces a coherent signal log | **Met, and the answer is "no signal"** | §2.5 |

Raw output is in [`phase-5-evidence/`](phase-5-evidence/).

## 2. Evidence

### 2.1 Gate

[`phase-5-evidence/gate.txt`](phase-5-evidence/gate.txt).

```
cargo fmt --all -- --check                  exit=0
cargo clippy --all-targets -- -D warnings   exit=0
cargo test                                  exit=0
    unittests src/lib.rs   35 passed   (+11: config 4, registry loading 7)
    tests/book.rs           9 passed
    tests/engine.rs         8 passed   (new)
    tests/replay.rs        10 passed
    tests/snapshot_sides.rs 3 passed
    tests/solver.rs        18 passed
    tests/stage2.rs         8 passed
cargo build --release                       exit=0
```

### 2.2 What ships

`src/config.rs` reads `config/example.toml` into `Config`: the `[engine]` block
deserializes straight into `SolverConfig`, per-venue settings carry the metadata
URL, cache directory, and fee multiplier, and `[registry]` says where the graph
lives. Unknown keys are rejected.

`src/registry.rs` gains the loading half: `EventId`, `CanonicalEvent`,
`ContractBinding`, `Registry::from_toml`, and `validate_against_markets`. Loading
is pure — no clock, no network — and enforces every invariant checkable from the
file. `src/engine.rs` is new. `src/book.rs` gains `apply_and_mark`, and `Feed`
gains a defaulted `request_resync`.

`config/registry.toml` ships a real graph: the 80 BTC strikes from the phase 3
session D capture, as 80 complement groups and one 80-rung monotone ladder. The
CLI gains `scan --live`, `scan --replay <file>`, and `registry validate`.

```
$ sum100 registry validate
events     1
  btc-2026-09-14-17        resolves 2026-09-14T21:00:00+00:00  Bitcoin price at 2026-09-14 17:00 ET
groups     81 (complement 80, exhaustive 0, monotone 1, implies 0, equivalent 0)
contracts  kalshi 80, polymarket 0
registry OK
```

### 2.3 Dirty marking and the loop

`apply_and_mark` returns `(Applied, Vec<ContractId>)`. A snapshot or an applied
delta marks its contract; a gap, a disconnect, a skipped delta, and an unknown
contract mark nothing, because every book they touch is no longer `Live` and the
solver would reject each resulting group as `not_live`. The dirty set is an
ordered `Vec` so replay evaluates in the same sequence the live run did.

`the_loop_solves_dirty_groups_and_hands_signals_to_the_sink` drives four
snapshots into the engine. The last one breaks two constraints at once — the
contract's own yes and no cost 95 together, and buying rung 2 at 40 while rung 3
bids 60 crosses the ladder — and both come out, ranked, with their exact costed
numbers. `each_dirty_group_is_evaluated_exactly_once_per_update` pins criterion 3:
four updates, each touching one complement group and the shared 4-rung ladder,
produce exactly eight evaluations.

### 2.4 Replay determinism

`replaying_a_recorded_session_twice_produces_identical_output` runs the phase 3
session A capture through the engine twice and compares the serialized state
stream, not just the fields the test happens to name — that JSON is exactly what
the API layer will publish. Signal logs, state counts, engine metrics, and solver
metrics all match. Nothing time-dependent leaks in because engine timestamps come
from the injected replay clock.

### 2.5 The interesting result

Two real sessions, same 80-contract registry.

| Session | Events | Books updated | Groups evaluated | Candidates | Signals | Rejections |
|---|---|---|---|---|---|---|
| phase 2.1 E (healthy) | 14,460 | 14,460 | 28,920 | 0 | 0 | 79 not live, 14,225 stale |
| phase 3 D (reconnect storm) | 38,659 | 20,352 | 40,704 | 0 | 0 | 17,933 not live, 34 stale |

Three things fall out of this.

**The market was coherent.** 14,460 complement evaluations on live Kalshi BTC
books produced zero candidates, and `crossed_books_observed` is zero for the whole
session. Nothing was mispriced. That is the predicted result, and the engine
saying so plainly is the point.

**The freshness gate is all-or-nothing, and that bites wide groups.** The 80-rung
ladder needs all 80 books inside `max_book_age_ms` simultaneously. Far strikes
tick rarely, so at the shipped 500 ms the ladder was evaluable 156 times out of
14,460 — about 1%. Raising the budget to 60 s only moved stale rejections from
14,225 to 5,565 and still found nothing, so the gate was not hiding a signal here.
But the policy means group evaluability decays sharply with member count, and a
wide ladder over illiquid wings is effectively invisible. Worth a per-group age
budget, or gating on the members a candidate actually touches rather than the
whole group. Left as is for now; it is a policy question, not a bug.

**The degraded session is correctly refused.** Phase 3 session D is the reconnect
storm — 18,160 snapshots against 2,192 deltas, every book ending `Resyncing`. The
engine evaluated it and declined 17,933 times for `not_live`. A signal log with
nothing in it is the right output for a session whose books could not be trusted.

## 3. Where the implementation departs from the written spec

**Loading does not call the venue.** The spec has `Registry::from_toml` resolve
tickers to contract ids via REST. Contract ids come from interning, which is
deterministic and needs no network; making the loader fetch would break the
tested property that replay runs with no credentials and no network, and would
let a venue outage change the id assignment a recording was made under.
Structural loading is offline; `registry validate --live` fetches metadata,
caches it under `cache_dir`, and reports every ticker the venue does not list.

**`Registry.groups` stays a `Vec`, not a `HashMap<GroupId, _>`.** Group ids are
dense and assigned sequentially, so a `Vec` indexed by id is the same O(1) lookup
with less overhead. The accessors (`group`, `groups_for`) are identical.

**No HTTP API and no `PaperExecutor`.** The spec's `main` sketch wires
`api::serve` and a paper executor, but its own scope section defers both. The
engine publishes `EngineState` on a `tokio::broadcast` and writes signals to an
`OpportunitySink` trait; those are the two seams. `EngineState` is `Serialize`,
so the API layer is a serializer, not a translation.

**`EngineState` carries book summaries, not books.** State publishes on every
applied event. Copying a hundred levels per contract per delta would cost more
than the solve it follows, so each contract contributes state, sequence, best bid,
best ask, and age. Publishing is skipped entirely when nothing is subscribed.

**`Engine` is generic over its sink, not boxed.** `Box<dyn OpportunitySink>` would
have made the signal log unreadable afterwards without a lock in the hot path,
which ARCHITECTURE §5 rules out.

**Gap handling moved onto the `Feed` trait.** `request_resync` is a defaulted
trait method: live drops the socket, replay does nothing, because a recording
already contains whatever resync the live run performed and inventing a second
one would desynchronize it.

## 4. Open

- **`scan --live` is unrun.** It shares `KalshiFeed` with `dump`, which has
  production evidence, but the engine path itself has not been pointed at a live
  socket. Needs credentials and an open market.
- **Group evaluability under the freshness gate**, per §2.5. The current policy
  makes wide ladders nearly invisible.
- **No live candidate log yet.** Phase 4's deliverable wanted real violations with
  a rejection breakdown; the breakdown now exists and is reportable, but both
  captures in hand are coherent, so there are no accepted signals from real data.
- **Tick-to-signal latency is still unmeasured.**
- The API layer, the paper executor, and Polymarket, as scoped.
