# Sum100 Architecture

Version 1.2 — reconciled with phase 2 (book store) on September 13, 2026.
Status: target architecture; implementation is through phase 2 (book store).

The imported design originally used the working name Parity. The repository and
binary remain Sum100. The current runnable interface and verification evidence
are in [README.md](README.md); [PLAN.md](PLAN.md) separates completed work from
future milestones. Later examples in this document (registry, replay, API,
frontend) describe target components not yet present.

---

## 1. Context and goals

Sum100 watches prediction market order books and detects when the prices of logically related contracts are mutually inconsistent. Detection alone is not useful. The system must also determine whether an inconsistency is executable after fees, after walking real order book depth, and after accounting for how long capital stays locked.

### Functional goals

1. Maintain accurate live order books for thousands of contracts across two venues.
2. Model logical relationships between contracts as a constraint graph.
3. Detect constraint violations in single-digit milliseconds from the triggering price update.
4. Cost each violation correctly using venue-specific fee schedules and available depth.
5. Rank surviving opportunities by annualized return on locked capital.
6. Simulate execution and track profit and loss without placing real orders.
7. Replay any recorded session through the identical code path used live.

### Non-functional goals

| Property | Target | Rationale |
|---|---|---|
| Tick to signal p99 | under 5ms | Edges on these venues persist for seconds, not microseconds, so this is comfortable headroom rather than a race |
| Correctness of emitted signals | zero false positives from stale or gapped books | A wrong signal is worse than a missed one |
| Memory | bounded regardless of session length | The process must run for weeks |
| Replay determinism | byte-identical output for identical input | Otherwise regression testing is impossible |
| Single process | no external message broker or database required to run | Operational simplicity is a feature |

### Explicit non-goals

- Placing real orders. The executor is a simulator by design, not by omission.
- Predicting event outcomes. Sum100 has no view on whether the Fed cuts rates.
- Supporting more than two venues initially. The feed trait makes adding a third cheap, but breadth before depth would be a mistake.
- Horizontal scaling. One process comfortably handles the entire universe of contracts on both venues. Distributing it would add failure modes and buy nothing.

---

## 2. System overview

```
                                  +-------------+
                                  |  recorder   |
                                  +------^------+
                                         |
  +-------------+     +-------------+    |    +-------------+     +-------------+
  | kalshi feed |---->|             |----+    |             |     |   paper     |
  +-------------+     | book store  |-------->|   solver    |---->|  executor   |
  | poly feed   |---->|             |         |             |     |             |
  +-------------+     +-------------+         +------^------+     +------+------+
  | replay feed |                                    |                   |
  +-------------+                             +------+------+            |
                                              |  registry   |            v
                                              +-------------+     +-------------+
                                                                  |  api layer  |
                                                                  +-------------+
```

Data flows in one direction. No component calls backwards into the one that feeds it. This is what makes replay trivially correct and makes the concurrency model simple enough to reason about without a whiteboard.

### Stage responsibilities

| Stage | Owns | Does not own |
|---|---|---|
| Feed | Network sockets, venue message formats, authentication, reconnection | Any notion of what a price means |
| Book store | Current order book state, sequence continuity | Which contracts relate to which |
| Registry | Contract identity, relationships, resolution metadata | Any live price |
| Solver | Constraint evaluation, fee application, depth walking | Network, persistence |
| Executor | Simulated positions, fills, profit and loss | Whether an opportunity is valid |
| Recorder | Durable raw message log | Interpretation of messages |
| API | Serialization for the frontend | Any calculation |

---

## 3. Data model

### 3.1 Money

Every monetary quantity is an `i64` count of cents. Floating point is banned throughout the money path.

```rust
pub type Cents = i64;
```

The reason is specific rather than stylistic. Arbitrage detection is a comparison against an exact threshold. A price sum of `99.99999999999999` versus `100.0` is the difference between a signal and silence, and IEEE 754 addition of decimal fractions produces exactly that kind of residue. Integer cents make the comparison exact and make all fee arithmetic reproducible across machines.

Kalshi currently sends decimal-dollar strings. The parser converts directly to
integer cents and rejects nonzero sub-cent precision rather than rounding it.
Sizes floor to whole contracts with discarded fractions counted in exact
hundredths; signed deltas use mathematical floor. The raw recording retains the
original strings, so future precision changes remain diagnosable and replayable.

### 3.2 Core types

```rust
pub enum Venue { Kalshi, Polymarket }

pub struct Level {
    pub price: Cents,   // 0..=100, exact cent ticks only
    pub size: i64,      // contracts available at this level
}

pub struct Book {
    pub venue: Venue,
    pub contract_id: ContractId,
    // Dense size-by-price arrays (0..=100); asks() derives yes asks as 100 - no.
    yes: [i64; 101],
    no: [i64; 101],
    pub seq: u64,
    pub updated_at_ms: u64,
    pub state: BookState,
}

pub enum BookState {
    Live,           // snapshot applied, deltas in sequence
    Resyncing,      // gap detected, awaiting fresh snapshot
    Uninitialized,  // subscribed, no snapshot yet
}
```

`BookState` is load-bearing. The solver refuses to evaluate any group containing a book that is not `Live`. This single check eliminates the most dangerous class of false signal, which is trading on a book that silently diverged from the venue's true state. Floor drift that would drive a level negative is clamped to zero and counted; a crossed book (`best yes bid + best no bid > 100`) stays `Live` and is counted for the complement fast path.

### 3.3 Registry types

```rust
pub struct CanonicalEvent {
    pub id: EventId,
    pub description: String,
    pub resolves_at: DateTime<Utc>,
    pub resolution_source: String,
    pub resolution_rules_hash: u64,
}

pub struct ContractBinding {
    pub contract_id: ContractId,
    pub venue: Venue,
    pub venue_ticker: String,
    pub event: EventId,
    pub side: Side,
    pub verified: bool,
}

pub enum Relation {
    Complement { contract: ContractId },
    Exhaustive { members: Vec<ContractId> },
    Implies { antecedent: ContractId, consequent: ContractId },
    Equivalent { a: ContractId, b: ContractId },
}

pub struct ConstraintGroup {
    pub id: GroupId,
    pub relation: Relation,
    pub members: Vec<ContractId>,
}
```

`verified` defaults to false. An unverified binding never produces a cross-venue signal. Promotion to verified requires a human confirming that both contracts settle on the same source, at the same time, with the same tie-handling. This is discussed further in section 7.

---

## 4. Component design

### 4.1 Feed layer

The feed layer exists to make everything downstream venue-agnostic. It is defined by one trait.

```rust
pub trait Feed: Send {
    fn next(&mut self) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<FeedEvent>> + Send + '_>
    >;
}

pub enum FeedEvent {
    Snapshot { contract: ContractId, yes: Vec<Level>, no: Vec<Level>, seq: u64, ts_ms: u64 },
    Delta    { contract: ContractId, side: Side, price: Cents, size_delta: i64, seq: u64, ts_ms: u64 },
    Disconnected { venue: Venue },
    Resubscribed { contract: ContractId },
}
```

KalshiFeed implements this trait now. PolymarketFeed and ReplayFeed are planned.
Snapshots preserve both resting outcome sides; deltas preserve wire yes/no and
signed changes. The feed performs no no-price complement conversion. Phase 2's
book store owns book state and `100 - P`. Snapshot timestamps without a
venue timestamp use the recorded local receipt time.

**KalshiFeed.** Holds a websocket connection to Kalshi's trade API. The order book delta channel is private and requires request signing with an RSA key, so the feed constructs headers containing a key id, a timestamp in milliseconds, and a signature over the concatenation of timestamp, method, and path. Every WebSocket handshake requires signing, including public ticker and trade channels. Demo and production require separate credentials; demo remains the default and production requires `--prod`. Kalshi sends a full snapshot on subscription and incremental deltas thereafter, each carrying a sequence number.

**PolymarketFeed.** Polymarket exposes three separate APIs. Gamma provides public market discovery and metadata. The CLOB provides the live order book and requires wallet-based signing only for order placement, not for reading. A separate data API provides historical activity. Sum100 uses Gamma for discovery and the CLOB websocket for live books. Note that the CLOB migrated to a V2 contract in April 2026, changing a substantial portion of the order struct and the collateral token, so any older integration example should be treated as wrong.

**ReplayFeed.** Reads a recorded file and yields the same `FeedEvent` values in the same order, with the same timestamps. Optionally paces them to wall clock for realistic latency measurement, or runs as fast as possible for regression testing.

The trait boundary is the single most important architectural line in the system. Everything above it is I/O and venue trivia. Everything below it is deterministic logic.

#### Reconnection policy

Disconnections are expected, not exceptional. On disconnect the feed:

1. Emits `Disconnected` so the book store can mark every affected book `Resyncing`.
2. Backs off exponentially starting at 250ms, capped at 30 seconds, with jitter.
3. On reconnect, resubscribes to all tracked contracts.
4. Emits `Resubscribed` followed by fresh `Snapshot` events.

No signal is emitted from any book that has not received a post-reconnect snapshot.

### 4.2 Book store

The book store applies `FeedEvent` values to in-memory book state. It is the only component that mutates books. It is implemented for Kalshi in `src/book.rs` and driven by the `dump` CLI.

Its central responsibility is sequence continuity at the venue subscription scope. For Kalshi, sequence numbers can span multiple tickers on one subscription; per-contract checks alone would report false gaps. If a subscription receives sequence `n + 2` when it expected `n + 1`, the affected local books can no longer be trusted. The response is unconditional:

1. Set `state = Resyncing` on every live book; leave level contents unchanged.
2. Increment the `sequence_gaps` counter.
3. Return `Applied::Gap` so the driver can call `KalshiFeed::request_resync()`, which drops the socket and reuses the existing reconnect / resubscribe path (Kalshi has no per-ticker snapshot request).
4. Reject all delta application until a snapshot arrives and rebases the expected sequence.

Snapshots are absolute and always applied. There is no attempt to repair or interpolate. The cost of a wrong book is a false signal that would lose money, and the cost of a brief blind spot is one missed opportunity out of thousands. Sequence tracking is currently for the single orderbook subscription the feed opens; multiple concurrent `sid`s are not yet modeled.

After applying an update, a future registry will mark every constraint group containing that contract as dirty and hand the dirty set to the solver. Dirty marking is not yet implemented.

### 4.3 Registry

The registry is a load-time structure built from `config/registry.toml` plus discovery calls to each venue's market listing endpoint.

```toml
[[event]]
id = "fed-2026-09"
description = "FOMC rate decision, September 2026"
resolves_at = "2026-09-17T18:00:00Z"
resolution_source = "FOMC statement"

[[event.group]]
type = "exhaustive"
members = [
  { venue = "kalshi", ticker = "KXFED-26SEP-C50" },
  { venue = "kalshi", ticker = "KXFED-26SEP-C25" },
  { venue = "kalshi", ticker = "KXFED-26SEP-NC" },
  { venue = "kalshi", ticker = "KXFED-26SEP-H" },
]

[[event.group]]
type = "equivalent"
verified = false
members = [
  { venue = "kalshi", ticker = "KXFED-26SEP-C25" },
  { venue = "polymarket", token = "0x..." },
]
```

Keeping this as configuration rather than code means adding coverage is a pull request against a data file, not a refactor. It also means the mapping can be reviewed independently of the engine.

#### Group indexing

At load time the registry builds a reverse index from contract to the groups containing it. This index is what makes dirty marking cheap. A price update on one contract typically touches one or two groups out of thousands, and finding them is a single hash lookup rather than a scan.

### 4.4 Solver

The solver receives a dirty set and evaluates each group.

#### Fast paths

Four constraint shapes cover the overwhelming majority of real groups, and each has a closed-form check that runs in tens of nanoseconds.

**Complement.** For a single contract, if the yes bid plus the no bid exceeds 100 cents, selling both locks a profit.

**Exhaustive set.** Sum the best asks across all members. If the total plus fees is under 100 cents, buying one of each guarantees a dollar.

**Monotonicity.** For a ladder ordered by threshold, prices must be non-increasing. Any adjacent inversion is a violation, capturable by buying the cheaper stronger claim and selling the dearer weaker one.

**Cross venue equivalence.** For a verified pair, if buying yes on one venue and no on the other costs under 100 cents combined, the position is riskless.

#### General fallback

Some clusters do not reduce to any of the four shapes. Overlapping partial partitions, conditional markets, and multi-leg combinations produce constraint systems that need a general method.

The general formulation treats the world as a finite set of mutually exclusive states. Each contract becomes a payoff vector over those states, holding one in the states where it resolves yes and zero elsewhere. A price vector is arbitrage-free if and only if there exists a probability distribution over the states under which every contract's price equals its expected payoff. This is a linear feasibility problem. When it is infeasible, the dual solution yields a portfolio with non-negative payoff in every state and strictly negative cost, which is the trade.

This is elegant but expensive relative to the fast paths, so it runs only for groups the fast paths cannot express, and it is deliberately the last thing implemented.

#### Costing

A violation is a candidate, not a signal. Costing turns one into the other.

1. **Depth walk.** For each leg, consume ask levels from the top down until the requested quantity is filled or the book is exhausted. Record the true cost, not the top-of-book price times quantity.
2. **Size capping.** Executable quantity is the minimum fill across all legs. A four leg set where one leg has only 40 contracts available is a 40 contract trade regardless of the others.
3. **Fee application.** Apply the venue fee model per level consumed. Rounding up per level slightly over-counts relative to venues that round once per order. Over-counting is deliberately the safe direction.
4. **Freshness gate.** Every participating book must be `Live` and updated within `max_book_age_ms`.
5. **Net edge.** Guaranteed payoff minus total cost minus total fees. If this is not strictly positive, discard.
6. **Ranking.** Compute annualized return as net divided by capital, scaled by 365 over days to resolution.

### 4.5 Fee models

```rust
pub trait FeeModel {
    fn taker_fee(&self, price: Cents, qty: i64) -> Cents;
}
```

**Kalshi.** The published schedule is round up of `M x 0.07 x C x P x (1 - P)` where P is price in dollars, C is contract count, and M is a per-series multiplier defaulting to one. Expressed in integer cents with price as an integer out of 100, this becomes a ceiling division of `7 x C x price x (100 - price) x M` by `10000`. Maker fees use a substantially lower coefficient, which matters if the executor is ever extended to model passive orders.

**Polymarket.** The taker fee is probability dependent with a peak around 1.80 percent near 50 cents, tapering toward zero at the extremes. The critical operational detail is that the published documentation and the live per-token fee rate endpoint have disagreed in the past, with the endpoint returning values for some market categories that match neither the documented rate nor each other. The fee model therefore reads the live endpoint per token and caches the result, and the hardcoded curve exists only as a fallback with a loud warning.

Getting fees wrong does not produce a subtle inaccuracy. It produces a system whose every output is wrong in the same direction, which is worse, because the outputs still look plausible.

### 4.6 Paper executor

The executor consumes opportunities and simulates the resulting trades.

- Fills are taken against the book snapshot that produced the signal, level by level.
- Each leg records whether it would have filled completely, partially, or not at all.
- A trade where any leg fails to fill is recorded as a legging failure, with the resulting unhedged directional exposure marked explicitly. This is the real world's most common way of turning an arbitrage into a bet.
- Positions accrue until the event's resolution timestamp, at which point settlement is simulated.
- Profit and loss is tracked both realized and mark to market.

Legging failure rate is one of the most honest metrics the system produces and belongs in any writeup of results.

### 4.7 Recorder

The recorder writes every inbound raw WebSocket message **before parsing**, as
gzipped newline-delimited receipt envelopes, one file per venue per UTC receipt
day, separated by environment. It does not serialize `FeedEvent` or parsed venue
JSON. Each envelope contains local receipt milliseconds, a monotonic recording
sequence, message kind, and a raw string (base64 payload for non-text messages).
Only one trailing LF is removed from text. Decoding the envelope restores the
original payload bytes under that single-LF rule. Completed gzip members are
flushed and synced every two seconds and at graceful shutdown. An OS lock
prevents concurrent writers; append validates the existing daily corpus and
continues its sequence. See README for crash-tail recovery and reader details.

File format is deliberately boring. It is greppable, streamable, compresses well, and requires no schema migration. When the corpus grows large enough that analysis is slow, the answer is to load it into Polars or DuckDB from a Python notebook, not to build query infrastructure in Rust.

### 4.8 API layer

An `axum` server exposes two endpoints.

- `GET /ws` streams engine state as JSON at a fixed cadence of roughly ten updates per second. The payload contains current constraint group states, live opportunities, and health counters.
- `GET /api/signals?since=<ts>` returns historical signals for the frontend's table and charts.

The API performs no calculation. Every number it serializes was computed by the solver. If the frontend ever needs a value the API does not provide, the fix is to compute it in the engine, not in TypeScript.

---

## 5. Concurrency model

Sum100 runs as a single `tokio` process with a small number of long-lived tasks connected by bounded channels.

```
  [venue receive] --> [raw recorder] --> [venue parser] -- bounded mpsc --> [engine]
                                                                          |
                                                                     [future API]
```

### Ownership

Each piece of mutable state has exactly one owner.

| State | Owner |
|---|---|
| Websocket connections | Feed tasks |
| Order books | Engine task |
| Constraint groups and dirty set | Engine task |
| Positions and PnL | Engine task |
| Output file handles | Recorder owned by the feed worker today; a separate recorder task is a future optimization |
| Connected websocket clients | API task |

No mutexes appear anywhere in the hot path. The engine task owns the book store, registry, solver, and executor together, because they are called in strict sequence on every update and splitting them across tasks would add channel hops for no parallelism gain.

### Backpressure

Channels are bounded. The current feed-to-consumer channel holds at most 256
events and uses lossless backpressure. A full channel blocks the producer.
Raw recording happens before event delivery, and recording errors stop the feed.

**Never overwrite or coalesce raw signed deltas.** They are changes, not complete
states: dropping an intermediate delta corrupts the resulting book. Coalescing
is only a possible future policy for complete derived book views or dirty-group
notifications after every delta has been applied. Phase 2 must track subscription
sequence continuity (which can span multiple tickers) and invalidate books on a
gap; it must not infer per-ticker continuity from a shared subscription sequence.

The recorder must not drop data. Compression and disk writes are currently
synchronous within the owning feed worker. This preserves ordering and bounds
memory; its performance has not been benchmarked. A future dedicated recorder
worker must use a bounded, lossless queue, propagate errors, and drain on shutdown.

### Why not a thread per venue with shared state

Considered and rejected. Shared books behind a read-write lock would let the solver run concurrently with feed processing, but the solver's work per update is microseconds. Lock acquisition would dominate the work being parallelized, and the design would trade a simple single-owner model for a category of bug that is genuinely hard to test for.

---

## 6. Replay design

Replay is not a mode. It is a different implementation of the `Feed` trait.

```rust
let feed: Box<dyn Feed> = match args.source {
    Source::Live   => Box::new(KalshiFeed::connect(&cfg).await?),
    Source::Replay => Box::new(ReplayFeed::open(&args.path)?),
};
engine.run(feed).await
```

Everything downstream is identical. This has three consequences worth stating explicitly.

1. **Backtests exercise production code.** There is no second implementation of the solver to drift out of sync with the first.
2. **Development does not require market hours or credentials.** Most of the work happens against a recorded file.
3. **Regression testing is possible.** A checked-in corpus plus an expected signal log turns any behavior change into a visible diff.

For determinism, the engine's clock is injected rather than read from the system. Under replay it advances according to recorded timestamps. Under live it reads the wall clock. Without this, freshness gates would behave differently in replay and the output would not be reproducible.

---

## 7. Failure modes and how they are handled

| Failure | Detection | Response |
|---|---|---|
| Websocket disconnect | Read error or ping timeout | Mark books resyncing, exponential backoff reconnect |
| Sequence gap | Expected sequence mismatch | Mark book resyncing, request snapshot, increment counter |
| Stale book during fast move | `updated_at_ms` beyond threshold | Reject signal, increment stale rejection counter |
| Venue schema change | Deserialization failure | Log the raw payload, skip the message, increment parse error counter, do not crash |
| Mismatched resolution rules | Manual verification required before `verified` is set | Unverified pairs never emit cross-venue signals |
| Fee endpoint unavailable | HTTP error or timeout | Fall back to the hardcoded curve, log a warning, mark signals as using a fallback fee |
| Clock skew between venues | Compare venue timestamps against local receipt time | Track offset per venue, widen freshness threshold if skew exceeds tolerance |
| Engine falls behind | Channel at capacity | Apply lossless bounded backpressure; never coalesce signed deltas |
| Memory growth | Book count and level count metrics | Bounded by contract universe, alert if level vectors grow without bound |

### The resolution rules trap

This deserves separate treatment because it is the failure that will actually cost money in a live version and the one that is least obvious.

Two contracts can have nearly identical titles and settle completely differently. Consider a market on whether a government shutdown occurs. One venue may resolve on whether appropriations lapse at midnight Eastern. Another may resolve on whether a shutdown lasts more than one full business day. Another may use a different data source that publishes at a different time. A contract about a sports outcome may differ on how overtime or a postponement is handled.

If Sum100 treats two such contracts as equivalent, it will report an arbitrage that is not an arbitrage. It is a directional bet on the difference between two resolution rules, taken unknowingly, at unfavorable odds.

The mitigation is procedural rather than algorithmic. Candidate matches are generated automatically using title similarity and resolution date proximity. They are then presented for review with a side-by-side diff of both venues' resolution language. A match becomes `verified` only after explicit confirmation, and only verified matches can produce cross-venue signals. A hash of the resolution text is stored, so if either venue edits its rules, verification is automatically revoked.

---

## 8. Observability

Metrics collected continuously and exposed both on the health endpoint and in the frontend health strip.

**Throughput.** Messages received per second per venue. Book updates applied per second. Constraint groups evaluated per second.

**Latency.** Histogram of the interval from feed event receipt to signal emission. Reported at p50, p95, and p99. This is the headline performance number.

**Correctness.** Sequence gaps per hour. Parse errors per hour. Reconnections per hour. Books currently in the `Resyncing` state.

**Signal quality.** Candidate violations detected. Rejections by reason, broken out into stale, insufficient depth, fee exceeds gap, and annualized return below threshold. The ratio of candidates to accepted signals is the clearest single indicator that the costing layer is doing real work.

**Execution.** Simulated fills, legging failures, realized profit and loss, capital currently locked, weighted average days to resolution across open positions.

---

## 9. Decision records

### ADR-001: Rust over Python or Go

**Status.** Accepted.

**Context.** The engine must maintain thousands of order books under continuous update with predictable latency, and must never produce a wrong number due to a type or arithmetic error.

**Options.**

| Option | Complexity | Latency | Ecosystem fit | Learning value |
|---|---|---|---|---|
| Python | Low | Poor under load, GIL contention | Excellent, official SDKs | Low |
| Go | Medium | Good, GC pauses tolerable here | Good | Medium |
| Rust | High | Excellent, no GC | Adequate, community SDKs | High |

**Decision.** Rust.

**Reasoning.** The latency requirement alone would be satisfied by Go. The deciding factors are the type system and the ownership model. Modeling constraint relations as enums with exhaustive matching means an unhandled constraint type is a compile error rather than a runtime surprise. The single-owner concurrency model falls out of the borrow checker rather than being a convention that erodes. Money uses the `Cents` alias for `i64`; conversion from venue decimals is explicit. A distinct monetary newtype remains a possible later type-safety improvement.

**Consequences.** Slower initial development. Fewer official venue SDKs, so more client code is hand-written, which is acceptable because the book handling is the part worth owning anyway.

### ADR-002: Single process, no message broker

**Status.** Accepted.

**Context.** The natural instinct for a multi-stage streaming pipeline is to put a broker between stages.

**Options.** In-process channels; Redis streams; Kafka.

**Decision.** In-process bounded channels.

**Reasoning.** The entire contract universe across both venues is in the low thousands. Peak message rate is well within what a single core can parse. A broker would add a network hop, a serialization round trip, an operational dependency, and a new failure mode, in exchange for a scaling headroom the system will never approach. The correct engineering answer to a capacity question is to measure first, and the current single-process design is the starting point; throughput and latency claims require the phase 10 benchmarks.

**Consequences.** Restarting the process loses in-flight state, which is acceptable because books resynchronize from snapshots within seconds. Scaling beyond one process would require real work, which is the right trade when that day is unlikely to arrive.

### ADR-003: Replay as a feed implementation rather than a separate backtester

**Status.** Accepted.

**Context.** Historical validation is required. The conventional approach is a separate backtesting harness.

**Options.** Separate backtester sharing some library code; replay implementing the live feed interface.

**Decision.** Replay implements the `Feed` trait.

**Reasoning.** Separate backtesters diverge from production. The divergence is silent and is discovered at the worst possible time. Making replay a feed implementation costs one extra trait and guarantees the solver, costing, and executor code paths are identical.

**Consequences.** The clock must be injected rather than read globally. Recording becomes mandatory infrastructure rather than a nice-to-have. Both are worth it.

### ADR-004: Fast paths with a general LP fallback rather than LP everywhere

**Status.** Accepted.

**Context.** All constraint checks could be expressed as linear programs over a state space, which is uniform and provably complete.

**Options.** LP for everything; closed-form fast paths only; fast paths with LP fallback.

**Decision.** Fast paths with LP fallback.

**Reasoning.** LP for everything is elegant and too slow for the common case, where checking a four member exhaustive set is four additions and a comparison. Fast paths only is fast and incomplete, leaving irregular clusters unhandled. The hybrid gets nanosecond checks for the ninety-plus percent of groups that fit a known shape, and correctness for the remainder.

**Consequences.** Two code paths must agree. This is handled by property testing, which generates random price configurations and asserts that both paths reach the same verdict on groups the fast path can express.

### ADR-005: Integer cents throughout

**Status.** Accepted.

**Context.** Money representation.

**Decision.** `i64` cents, no floating point anywhere in the money path.

**Reasoning.** Arbitrage detection is an exact threshold comparison. Floating point residue from decimal fraction addition produces both false positives and false negatives at the margin, and the resulting bugs are irreproducible across machines. Venue prices are parsed from decimal strings without floating point. Nonzero sub-cent values are rejected explicitly; size flooring is a separate, measured boundary policy.

**Consequences.** Fee formulas must be rewritten as integer arithmetic with explicit ceiling division. Annualized return, which is a ratio rather than a money value, is the one place a float appears, and it is used only for ranking and display, never for a go or no-go decision.

### ADR-006: Frontend as a pure view

**Status.** Accepted.

**Context.** The web interface needs to display fees, edges, and rankings.

**Decision.** The frontend contains zero business logic. It renders values computed by the engine.

**Reasoning.** Duplicating the fee model in TypeScript creates two sources of truth that will disagree. A discrepancy between screen and engine is worse than having no screen, because it undermines trust in both.

**Consequences.** Adding a displayed metric requires an engine change and a protocol change. This friction is intentional.

---

## 10. Open questions

1. **Maker fee modeling.** The current executor models only taker fills. Passive orders pay substantially less on Kalshi but introduce queue position risk and fill uncertainty. Modeling this properly requires queue position simulation, which is a large enough piece of work to be its own project.

2. **Polymarket negative risk.** Certain multi-outcome Polymarket events support converting no positions across outcomes, which changes what constitutes a valid exhaustive set constraint in those markets. The exhaustive fast path is currently wrong for these and must either handle the mechanism explicitly or exclude negative-risk markets from the constraint graph.

3. **Cross-venue settlement timing.** Even for genuinely identical events, the two venues may settle at different times. Capital is locked until the later of the two, which affects the annualized return calculation. The current model uses the later timestamp, which is conservative but may be too conservative where one leg can be closed early into a liquid book.

4. **Automated match confidence.** Title similarity plus date proximity generates candidate pairs, but the precision of that heuristic is unmeasured. Building a labeled set of confirmed matches and non-matches would allow reporting a real precision and recall figure rather than an assertion.

5. **Corpus size for regression testing.** A larger corpus catches more regressions and slows the test suite. The current plan checks in a small representative slice and keeps the full corpus out of version control, but the right slice has not yet been chosen.
