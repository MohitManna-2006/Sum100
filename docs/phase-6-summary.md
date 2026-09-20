# Phase 6 summary: execution, portfolio, health, risk

Date: September 20, 2026 (UTC). Baseline: `99082b7`.

**This phase reverses a documented project constraint.** PLAN.md said "Real
order placement. Not now, not later." and the README said "there is no
order-placement code path and there won't be one." `src/exec/kalshi.rs` can now
POST an order. The line in PLAN.md is struck through rather than deleted, so a
reader can see it was a decision. Everything below is built on the assumption
that the reversal was intended — `PHASE6_EXECUTION_PLAN.md` in this repo says as
much — and on the principle that the two failure modes are not symmetric.

## 1. Exit criteria

| # | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | AtomicTrade places legs in parallel with timeout and cancellation | **Met, with a correction**: cancellation of a *filled* leg is impossible and is not attempted; see §3 | §2.2 |
| 2 | KalshiOrderClient places real orders, or paper fills in paper mode | **Code complete, unrun against the venue**: request shape is unit-tested, no live order has been placed | §4 |
| 3 | Portfolio tracks capital, positions, realized and unrealized PnL | **Met** | §2.3 |
| 4 | HealthMonitor blocks trades when the feed is stale or erroring | **Met** | §2.3 |
| 5 | RiskEngine enforces event, theme, and concurrency limits | **Met** | §2.3 |
| 6 | EngineState includes portfolio, health, risk summaries | **Met** | §2.4 |
| 7 | Engine loop: solve → filter → execute → track | **Met** | §2.2 |
| 8 | CLI accepts `trade --live` with capital and limit flags | **Met** | §2.5 |
| 9 | Paper mode: no real orders, fills against the book | **Met** | §2.2, §2.6 |
| 10 | 20+ new tests | **Met**: 35 new (26 unit, 9 integration) | §2.1 |
| 11 | `cargo test`, `clippy`, `fmt` clean | **Met** | §2.1 |

## 2. Evidence

### 2.1 Gate

[`phase-6-evidence/gate.txt`](phase-6-evidence/gate.txt).

```
cargo fmt --all -- --check                  exit=0
cargo clippy --all-targets -- -D warnings   exit=0
cargo test                                  exit=0
    unittests src/lib.rs   61 passed   (+26: health 4, portfolio 6, risk 4,
                                        exec 11, config 1)
    tests/book.rs           9 passed
    tests/engine.rs         8 passed
    tests/phase6.rs         9 passed   (new)
    tests/replay.rs        10 passed
    tests/snapshot_sides.rs 3 passed
    tests/solver.rs        18 passed
    tests/stage2.rs         8 passed
cargo build --release                       exit=0
```

`tests/replay.rs::no_wall_clock_reads_outside_clock_module` caught an early
draft of `exec/atomic.rs` reading a monotonic instant to log elapsed time. The
read was removed rather than the guard relaxed: fills are stamped from the
injected clock, so replay still reproduces a live run exactly.

### 2.2 The execution path

`src/exec/` is four files: the `OrderClient` trait and order types, `atomic.rs`
(multi-leg placement), `paper.rs` (simulated fills), `kalshi.rs` (the live
client). The trait uses boxed futures rather than `async-trait`, matching
`Feed`.

Legs go out concurrently through `join_all`. Sequential placement would price
the second leg off a book that has already seen the first, which is the move
that removes the edge; `both_legs_go_out_at_once_not_one_after_the_other`
asserts two placements are genuinely in flight together rather than trusting the
shape of the code.

Every leg is fill-or-kill. `an_edge_becomes_a_position_and_then_a_mark` drives a
crossed book through the whole loop: 200 contracts of yes at 55 and no at 40,
19,000 in premium and 683 in fees, two venue order ids, capital down 19,683, the
position marked at the bid, and 317 realized at settlement.

### 2.3 The gates

Health, then capital, then concentration — each with a counter and a test.

| Gate | Test | Counter |
|---|---|---|
| Venue not healthy | `an_unhealthy_venue_blocks_a_profitable_trade` | `blocked_unhealthy` |
| Cannot fund it | `a_trade_that_cannot_be_funded_is_skipped_not_half_placed` | `blocked_no_capital` |
| Event limit | `the_event_limit_refuses_the_trade_that_would_cross_it` | `blocked_risk_limit` |
| Day closed | `a_closed_day_stops_new_risk_and_midnight_reopens_it` | `blocked_no_capital` |

The staleness case is the one worth naming: the edge is real, the book is
present, and the only thing wrong is that nothing has arrived in five seconds.
An idle far strike and a dead socket produce identical books, which is exactly
why health is separate from the solver's freshness gate.

### 2.4 State

`EngineState` gains `portfolio`, `health`, and `risk`. `RiskStatus` reports the
railings even when nothing is blocking, so a dashboard can show them rather than
only their failures. Everything is `Serialize`, so the API layer stays a
serializer.

### 2.5 CLI

`trade` takes `--capital`, `--daily-loss-limit`, `--max-per-event`,
`--max-per-theme`, `--paper-mode`, and `--live-orders`. `scan` and `trade` are
the same loop; `trade` exposes the knobs.

Four gates, verified at the command line
([`phase-6-evidence/live-order-guards.txt`](phase-6-evidence/live-order-guards.txt)):

```
$ sum100 trade --replay ... --live-orders
Error: --live-orders needs --live; a recording cannot place orders
$ sum100 trade --live --live-orders
Error: --live-orders needs --prod
$ sum100 trade --live --prod --live-orders
Error: --live-orders needs executor.paper_mode = false in config/example.toml
$ sum100 trade --live --prod --live-orders --paper-mode
error: the argument '--live-orders' cannot be used with '--paper-mode'
```

There is no Makefile recipe that passes `--live-orders`. Spending real money is
something an operator types out in full.

### 2.6 Offline replay

[`phase-6-evidence/trade-replay.txt`](phase-6-evidence/trade-replay.txt). The
phase 2.1 session, 14,460 events, 28,920 group evaluations, **zero candidates,
zero trades placed, capital untouched**. The same result phases 4 and 5 reported:
the market was coherent. `replaying_a_real_session_in_paper_mode_places_nothing`
pins it as a test.

No recording in hand contains an arbitrage, so the execution path's correctness
rests on the nine integration tests rather than on real fills. That is a real
limitation, not a claim of coverage.

## 3. Where the implementation departs from the written spec

**Cancellation of filled legs is not attempted, because it is not possible.**
The spec's `execute` cancels "all successful fills" when a leg fails. A filled
order cannot be cancelled; it can only be unwound by trading back across the
spread. Doing what the spec says would log a successful cleanup while a live
one-sided position sat on the book. Instead every leg is fill-or-kill so the
venue refuses partials, and when a leg escapes anyway the result is
`ExecutionError::Legged` carrying the fills that need unwinding. A timeout is
its own variant: the futures are dropped, so the venue's state is genuinely
unknown, and `Timeout` says so rather than reporting "no fills".
`a_legged_trade_is_flagged_for_reconciliation_not_booked` asserts no cancel is
attempted and nothing is booked.

**`--paper-mode` does not opt into safety; `--live-orders` opts out of it.** The
spec has paper mode as a flag you pass to be safe. Forgetting a flag must not be
the difference between a simulation and real money, so paper is the default and
live requires four deliberate acts. `--paper-mode` is accepted and means what it
says; it is simply already true.

**The daily reset is calendar midnight, not `now + 86_400_000`.** The spec's
arithmetic resets 24 hours after whenever the process happened to start, so two
restarts in one day hand out two fresh loss budgets.

**`Portfolio::add_position` actually checks something.** The spec's daily-loss
check compares against `position.pnl_realized_cents.abs()`, which is zero for a
new position, so it can never fire. The check here is whether the day's realized
losses have already reached the limit. Capital is also genuinely debited on
entry and returned at settlement; the spec's `can_afford` never moved any.

**Positions mark at the bid.** The spec says "mark-to-market against current
bid" in a comment and the tests do not pin it. It is pinned here, including that
a leg with no bid marks at zero rather than at cost.

**The engine owns portfolio, health, and risk** rather than taking four mutable
references per step, matching ARCHITECTURE's single-owner model. `step` stays
synchronous and `execute` is the async half, so the decision path is still
testable without a runtime and a slow venue cannot delay book keeping.

**No `async_trait`.** `OrderClient` uses boxed futures like `Feed`, for the
reason `Feed` does.

## 4. Open

- **No real order has ever been placed.** `KalshiOrderClient`'s request shape is
  unit-tested against Kalshi's documented schema, but the endpoint has not been
  exercised. The first live run should be a single contract on a demo market.
- **Unwinding a legged position is not implemented.** The engine detects and
  reports; a human unwinds. `PaperOrderClient` refuses sells for the same reason.
- **Legging tolerance** is out by design (phase 7): both legs fill or the trade
  is refused.
- **Settlement is manual.** `Portfolio::realize` exists and is tested; nothing
  calls it automatically, because resolution feeds are phase 7.
- Tick-to-trade latency is still unmeasured.
- The freshness-gate concentration problem from phase 5 §2.5 is unchanged.
