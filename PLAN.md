# Sum100 Build Plan

Planning horizon: 14 weeks from start, 16 if the coherence engine is built.
The estimation path is specified in [docs/COHERENCE.md](docs/COHERENCE.md).

## Current checkpoint — September 20, 2026

The imported plan is a roadmap, not a claim that its future components exist.
Phase 1 stage 2, **phase 2 (book store)**, phase 3 (replay), phase 4 (solver
fast paths and costing), and **phase 5 (registry loading and the engine task)**
are implemented. Later session
decisions supersede the original `new_size`, bid/ask normalization,
unauthenticated public WebSocket, and parsed-event recorder assumptions.

- Foundations: existing fee/depth/freshness example tests, integer-cent parser,
  whole-contract floor parser, interned ContractId, and feed boundary are present.
  The fee ceiling invariant is now tested over all prices 1–99 and multiple sizes.
- Phase 1 stage 2: authenticated Kalshi stream, read-only REST discovery, raw
  pre-parse recording, bounded event delivery, reconnect logic, metrics, and CLI.
  Fixtures include original full payloads and a fresh snapshot plus 460 consecutive
  deltas, including 209 real no-side deltas.
- Phase 2: `Book` / `BookState` with dense yes/no size arrays, `100 - P`
  complement conversion, `BookStore` with subscription-scoped sequence tracking,
  gap detection, forced-reconnect resync via `KalshiFeed::request_resync`, book
  metrics, and the `dump` subcommand. Floor drift clamps at zero; crossed books
  stay `Live` and are counted. Fixture replay pins the final best bid/ask for
  the stage 2 live session.
- Validation: the fresh fixture capture ran 70 seconds (462 frames, 17,982 bytes).
  A subsequent 65-second run exercised the final recorder/lock implementation:
  277 frames, 14,507 compressed bytes, zero parse errors. These are stage 2 smoke tests, not an hour-long
  endurance test or a tick-to-signal benchmark. Offline book-store tests pass;
  the thirty-minute live side-by-side check against the Kalshi UI remains an
  operator verification step via `dump`.
- Phase 3: injected `Clock`, recorder control envelopes, `ReplayFeed`, pacing,
  and `replay --verify`; evidence in docs/phase-3-summary.md.
- Phase 2.1: one-sided Kalshi snapshots applied as empty sides (removes the
  false-gap reconnect loop) and `venue_ts_ms` on `FeedEvent`; evidence in
  docs/phase-2-1-summary.md. Retroactive phase 2 summary in docs/phase-2-summary.md;
  its thirty-minute UI gate remains open.
- Phase 4: in-memory registry with the contract-to-group reverse index, the four
  fast paths (complement, exhaustive, monotonicity, cross-venue), the costing
  pipeline (freshness gate, depth walk, thinnest-leg cap, per-level fees, net
  edge, annualized return), rejection-reason metrics, and four property tests.
  Evidence in docs/phase-4-summary.md, including four documented departures from
  the written spec. The general LP fallback stays future work. The phase's own
  live deliverable — a replayed trading day's candidate log with a rejection
  breakdown and five hand-checked rejections — needs phase 5 and remains open.
- Phase 5: `config.rs` (TOML config, unknown keys rejected), registry loading
  with events, bindings, and offline validation, `engine.rs` (dirty marking,
  solve, broadcast), and the `scan` and `registry validate` subcommands.
  `config/registry.toml` ships the 80-strike BTC graph from the phase 3 capture.
  Evidence in docs/phase-5-summary.md. Both recorded sessions replay coherent:
  zero candidates, which is the predicted result. `scan --live` is wired but has
  not been pointed at a live socket.
- Coherence engine: design accepted in [docs/COHERENCE.md](docs/COHERENCE.md);
  implementation is phases 9 and 10, not started.
- Pending: hour-long endurance session; thirty-minute UI side-by-side; a live
  `scan` run and a real candidate log; per-group freshness policy (a wide ladder
  is rarely evaluable at 500 ms); tick-to-signal latency; HTTP API; execution;
  second venue; coherence engine; UI.

Use [README.md](README.md) for working commands and setup. The supplied original
README is preserved verbatim as [README.reference.md](README.reference.md);
its future CLI commands and authentication claims are historical context.

---
Primary constraint: this is a side project running alongside coursework. Assume six to ten focused hours per week, not forty.

---

## 1. Scope and success criteria

### What done looks like

Sum100 is complete enough to defend in an interview when all of the following are true.

1. It connects live to at least one venue and maintains correct order books, with sequence gap handling that has been observed working in production rather than only in tests.
2. It detects real constraint violations on live data, and the detection log contains genuine findings rather than synthetic ones.
3. Its costing layer rejects the majority of candidate violations for concrete, logged reasons, and the rejection breakdown is reportable.
4. Any recorded session can be replayed through the identical code path and produces identical output.
5. Latency from feed event to signal is measured rather than estimated, with p50 and p99 both recorded.
6. The test suite includes property tests asserting the central no-arbitrage invariant.

Everything else is upside.

### The minimum shippable state

Phases 0 through 5 constitute a complete, defensible project. That is roughly four to five weeks. If the semester goes badly, stopping there produces something that still earns its place on a resume. Phases 6 onward increase the ceiling, not the floor. Phases 9 and 10 are the novel ceiling: a depth-weighted projection onto the registry's constraints, scored against resolved outcomes. They are not required for the floor.

### What deliberately stays out of scope

- Real order placement. Not now, not later.
- More than two venues.
- Maker fee and queue position modeling.
- Any machine learning component. There is no place in this system where a model would beat arithmetic, and adding one would be decoration. The coherence engine is a projection, not a model; it introduces no view.

---

## 2. Phase schedule

### Phase 0: Foundations
**Weeks 1**
**Goal.** Establish the shapes that everything else plugs into, before any I/O exists.

Tasks:
- Initialize the cargo workspace, configure clippy and rustfmt, add a GitHub Actions workflow running build, test, clippy, and fmt on push.
- Define `Cents`, `Venue`, `Level`, `Side`, `Book`, `BookState`, `ContractId`, `EventId` in `types.rs`.
- Define the `Feed` trait and the `FeedEvent` enum.
- Define the `FeeModel` trait.
- Implement `KalshiFees` with exact integer ceiling arithmetic.
- Write the first three tests: the 98 cent Fed example that must be rejected, the 95 cent version that must be accepted, and a fee arithmetic table test covering prices from 1 to 99.

**Deliverable.** A crate that compiles, has no I/O, and has passing tests encoding the core economic lesson.

**Done when.** `cargo test` passes and the fee table test matches the published schedule at every price point.

**Why this order.** Fee arithmetic is the thing most likely to be subtly wrong and least likely to be caught later, because wrong fees produce plausible-looking output. Pinning it down first with an exhaustive table test removes that risk permanently.

---

### Phase 1: Kalshi feed and recorder
**Weeks 2 to 3**
**Goal.** Get real data flowing and land it on disk.

Tasks:
- Implement Kalshi REST client for market and event discovery.
- Implement RSA request signing for authenticated endpoints.
- Implement the websocket client with subscription management.
- Probe ticker and trade payloads first to verify wire formats. All Kalshi WebSocket handshakes require credentials, including these public channels.
- Add authenticated order book delta subscription.
- Implement the recorder writing gzipped newline-delimited JSON, one file per venue per day.
- Add reconnection with exponential backoff and jitter.
- Add structured logging with `tracing`.

**Deliverable.** A binary that runs for an hour unattended and produces a data file that grows.

**Done when.** A file recorded over a market session can be read back and parsed without error. At the approved stage 2 checkpoint, fixture continuity is checked offline. Live sequence-gap detection and recovery are implemented and validated in phase 2.

**Risks.** Authentication is the most common place to lose a day. The signature covers the concatenation of timestamp, HTTP method, and path, and the timestamp is in milliseconds. Getting any of those three wrong produces the same opaque rejection. Test against the demo environment first.

---

### Phase 2: Book store
**Weeks 3 to 4**
**Goal.** Turn a stream of deltas into correct live prices.

Tasks:
- Implement snapshot application.
- Implement delta application with level insertion, update, and removal.
- Implement sequence tracking and gap detection.
- Implement the `Resyncing` state and snapshot re-request.
- Add metrics for messages applied, gaps detected, and books by state.
- Add a `dump` subcommand printing current best bid and ask for a given ticker, so correctness is visually verifiable against the Kalshi web interface.

**Deliverable.** Live prices in the terminal that match what the website shows.

**Done when.** Running side by side with the Kalshi web interface for thirty minutes shows no divergence, and any divergence that does appear was preceded by a logged gap.

**Why this matters more than it looks.** Every downstream number depends on book correctness. A subtly wrong book produces subtly wrong signals that look right. Spend the time here.

---

### Phase 3: Replay
**Weeks 4**
**Goal.** Stop depending on market hours.

Tasks:
- Implement `ReplayFeed` reading recorded files and emitting identical `FeedEvent` values.
- Add an injected clock trait with wall clock and replay implementations.
- Add pacing modes: real-time for latency measurement, as-fast-as-possible for regression runs.
- Verify that replaying a recorded session reproduces the same final book state as the live session did.

**Deliverable.** The ability to develop the solver at midnight on a Sunday against Tuesday's data.

**Done when.** Live and replay over the same session produce identical final book states and identical log output modulo timestamps.

**Why this is its own phase.** It is tempting to defer replay until the solver exists. Doing it first pays for itself within days, because every subsequent phase is developed and debugged offline against deterministic input.

---

### Phase 4: Solver fast paths
**Weeks 5 to 6**
**Goal.** Find real violations.

Tasks:
- Implement complement detection.
- Implement exhaustive set detection.
- Implement monotonicity detection for ladder markets.
- Implement depth walking with per-level fee accumulation.
- Implement the freshness gate.
- Implement size capping at the thinnest leg.
- Implement net edge calculation and the annualized return ranking.
- Add rejection reason tracking, broken out into stale, insufficient depth, fee exceeds gap, and below return threshold.
- Add property tests asserting that any emitted opportunity has non-negative payoff in every outcome and strictly negative cost.

**Deliverable.** A log of real candidate violations found on live Kalshi data, with the reason each was accepted or rejected.

**Done when.** A full trading day replayed produces a rejection breakdown, and spot-checking five rejections by hand confirms the engine's reasoning in each case.

**Expected finding.** Most candidates will be rejected for fees. This is the correct outcome and is the most interesting single result the project produces.

---

### Phase 5: Registry and configuration
**Weeks 6 to 7**
**Goal.** Stop hardcoding which contracts relate to which.

Tasks:
- Define the registry TOML schema.
- Implement loading and validation, failing loudly on malformed groups.
- Build the reverse index from contract to constraint groups.
- Implement dirty marking driven by that index.
- Add automatic discovery of exhaustive sets from Kalshi's event and market structure, since Kalshi already groups markets under events.
- Add a `registry validate` subcommand that checks every configured group against live market metadata.

**Deliverable.** Coverage expandable by editing a config file.

**Done when.** Adding a new event to the registry and restarting produces correct constraint checking on it with no code change.

**This is the minimum shippable state.** Everything through here is a complete project. Assess time remaining before continuing.

---

### Phase 6: Paper executor
**Weeks 7 to 8**
**Goal.** Turn signals into a track record.

Tasks:
- Implement simulated fills against the triggering book snapshot.
- Implement partial fill and legging failure detection.
- Implement position tracking with per-event aggregation.
- Implement simulated settlement at the resolution timestamp.
- Track realized profit and loss, capital locked, and weighted average days to resolution.
- Persist the signal and fill log to disk in a form loadable by a Python notebook.

**Deliverable.** A profit and loss curve over replayed history, with legging failure rate reported honestly.

**Done when.** A month of recorded data replays into a complete trade log with settlement, and the aggregate numbers are reproducible across runs.

---

### Phase 7: Polymarket
**Weeks 9 to 10**
**Goal.** Add the second venue.

Tasks:
- Implement Gamma client for market discovery.
- Implement CLOB websocket client for live books, targeting the V2 API.
- Implement the Polymarket fee model, reading the live per-token fee rate endpoint with the hardcoded curve as a logged fallback.
- Extend the recorder to a second venue.
- Add per-venue clock skew tracking.

**Deliverable.** Two live feeds recording simultaneously.

**Done when.** Both venues' books are live and correct at the same time for a full session, with independent gap and reconnection handling.

**Risk.** The CLOB V2 migration in April 2026 changed a large portion of the order struct and the collateral token, and older integration guides are actively misleading. Budget time for working from current documentation rather than examples.

---

### Phase 8: Cross-venue matching
**Weeks 10 to 11**
**Goal.** The headline capability.

Tasks:
- Implement candidate match generation using title similarity and resolution date proximity.
- Implement a resolution rules diff view presenting both venues' language side by side.
- Implement the verification workflow with a stored hash of resolution text.
- Implement automatic verification revocation when either venue's rules text changes.
- Implement cross-venue equivalence detection over verified pairs only.
- Report match precision against a hand-labeled set.

**Deliverable.** Cross-venue signals, with the verification gate making the false-match risk explicit rather than hidden.

**Done when.** At least twenty pairs are verified, and replay over recorded data produces cross-venue candidates with a documented rejection breakdown.

**This is the phase that differentiates the trading path.** It is also where the interesting interview material lives, because the resolution rules trap is the kind of problem that only appears when you actually build the thing. The estimation path, which is the novel ceiling, is phases 9 and 10.

---

### Phase 9: Coherence engine
**Weeks 12 to 13**
**Goal.** Publish the nearest internally consistent probabilities to the live books.

Depends on phase 5 (registry, dirty marking). Cross-venue primitives additionally depend on phase 8 (verified pairs). Steps 1 through 3 can start against single-venue groups as soon as phase 5 lands. Full design: [docs/COHERENCE.md](docs/COHERENCE.md).

Tasks:
- Quote summarization with the `WeightModel` trait and all four quality cases (two-sided, one-sided, crossed, empty). No fabricated midpoints.
- Weighted simplex projection onto exhaustive sets, with property tests.
- Weighted isotonic regression (pool adjacent violators) onto monotone ladders, with property tests.
- Cluster construction as connected components of the constraint graph, computed at registry load, with size logging and a hard cap.
- Dykstra over clusters, with a 50-iteration cap, residual reporting, a single-group short circuit, and a shuffle-order property test.
- Structural isolation from the solver: `src/coherence/` is not imported by `solver/`. No estimate reaches a go or no-go decision.

**Deliverable.** On a recorded session, single-group events produce a coherent probability vector that sums to one (or is flagged incomplete), and overlapping groups produce an order-independent projection with a published residual.

**Done when.** Property tests pass: constraints satisfied, idempotent, identity on already-feasible input, probabilities in `[0, 1]`, output independent of group order. Replay of a fixture session produces byte-identical estimates.

**Why this is its own phase.** This is the first novel component. The trading path is a careful implementation of known detection. The estimation path is a data product. Mixing the two numeric domains is the failure the rest of the design exists to prevent.

---

### Phase 10: Coherence publication and calibration
**Week 13**
**Goal.** Make the estimate consumable, durable, and scored. The harness is built in the same stretch as the projector, not later.

Tasks:
- Publication types (`CoherentEstimate`, `EstimateStatus`), snapshot endpoints, and a ~10 Hz coalesced websocket delta stream.
- Parquet storage partitioned by UTC day, written on change plus a short heartbeat, always on a status change. Persist the raw midpoint beside the coherent value.
- `model_version` on every row from day one.
- Calibration harness: Brier score and log loss for coherent versus raw midpoint, bucketed by time to resolution, scored only on `Coherent` rows, sliced by `model_version`.
- First scored comparison over whatever events have resolved in the recorded corpus.

**Deliverable.** A snapshot API, a history file, and a table that says whether coherent beat raw, by bucket.

**Done when.** A replay writes the same Parquet bytes twice, and the harness produces a Brier and log-loss comparison without reconstructing history by hand.

**Why this cannot wait.** Retrofitting storage means waiting weeks for new events to resolve. Either coherent beats raw, which is a publishable finding, or it does not, which tells you the weight model needs work. Both are real results. Neither exists without the history.

---

### Phase 11: Frontend
**Weeks 14 to 15**
**Goal.** Make the work visible in ten seconds.

Tasks:
- Add the `axum` API layer with a websocket state stream and a REST signal history endpoint.
- Scaffold Vite, React, and TypeScript.
- Build the coherence bar: a stacked bar of outcome prices against the 100 cent line, with the fee bar overlaid so the gap and the fee are visually comparable.
- Display the coherent estimate and the `shift` beside the raw midpoint, including `EstimateStatus` so flagged numbers are visible as flagged.
- Build the ladder monotonicity chart with violating segments highlighted.
- Build the edge versus days-to-resolution scatter with constant annualized return curves.
- Build the live signal table with leg breakdown.
- Build the health strip showing connection state, message rate, gap count, and latency percentiles.
- Apply the visual conventions: dark surface, monospace tabular figures, color reserved for direction, high density, updates that flash briefly and settle.

**Deliverable.** A screenshot and a short screen recording for the README.

**Done when.** The README opens with a visual that communicates the project without reading a word.

**Hard rule.** No business logic in the frontend. Every displayed number comes from the engine. The coherent estimate is rendered, never recomputed.

---

### Phase 12: Measurement and writeup
**Week 16**
**Goal.** Convert the build into evidence.

Tasks:
- Build the benchmark corpus and run `cargo bench`.
- Record tick to signal latency at p50, p95, p99.
- Record throughput in messages per second and constraint groups per second.
- Record the full candidate to signal funnel with rejection reasons.
- Record legging failure rate and simulated profit and loss.
- Record coherent-versus-raw Brier and log loss by time-to-resolution bucket.
- Fill in the README performance table with measured values.
- Write the results section: what was found, what was rejected and why, whether consistency improved forecasts, and what the numbers say about whether the trading edge is real.

**Deliverable.** Every number in the README is measured and reproducible by the reader.

---

## 3. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Kalshi authentication consumes days | Medium | Medium | Use credentials even for public WebSocket channels; demo requires its own key. Verify signature construction against a minimal script before integrating |
| Polymarket V2 documentation gaps | High | Medium | Treat all pre-May-2026 examples as wrong, work from current docs, defer this venue to phase 7 so the project is already viable without it |
| Fee model wrong in a way that is not obvious | Medium | High | Exhaustive fee table test in phase 0, read Polymarket rates from the live endpoint, never from documentation |
| Scope creep into real trading | Medium | High | The executor is a simulator by architecture. There is no order placement code path to accidentally enable |
| Frontend consumes disproportionate time | High | Medium | Scheduled after the coherence engine, deliberately. If time runs short, one static screenshot of the coherence bar is sufficient |
| Discovering no real arbitrage exists | Medium | Low | This is a finding, not a failure. A rigorous measurement showing edges are consumed by fees is a more interesting result than a strategy that appears profitable |
| Coursework crowds out the project | High | Medium | Phases 0 through 5 are the floor. Ship that, then reassess. Coherence is ceiling; do not start it by starving the registry |
| Venue changes its schema mid-build | Low | Medium | Parse failures log and skip rather than crash. The recorder captures raw payloads so any break is diagnosable after the fact |
| Coherent estimate leaks into the solver | Medium | High | Structural: `solver/` cannot import `coherence/`. Review any PR that shares a quote type across the two paths |

---

## 4. Metrics to collect

These are the numbers that turn the project into resume lines. Collect them deliberately rather than reconstructing them at the end.

**Scale.** Contracts tracked concurrently. Constraint groups maintained. Messages processed per second at peak. Total messages in the recorded corpus.

**Latency.** Tick to signal p50, p95, p99. Book update application time. Constraint group evaluation time.

**Correctness.** Sequence gaps detected and recovered. Reconnections handled. Parse errors. Books in resyncing state as a fraction of uptime.

**Signal funnel.** Candidate violations detected. Rejected for staleness. Rejected for insufficient depth. Rejected because fees exceed the gap. Rejected for annualized return below threshold. Accepted. The funnel ratios are the most substantive output of the whole project.

**Execution.** Simulated fills. Legging failures as a percentage. Realized profit and loss. Capital-weighted average holding period.

**Matching.** Candidate cross-venue pairs generated. Pairs verified. Pairs rejected on resolution rules mismatch. Precision against the hand-labeled set.

**Coherence.** Events with a coherent estimate. Mean absolute shift. Clusters that hit the iteration cap. Brier score and log loss for coherent versus raw midpoint, by days-to-resolution bucket, sliced by `model_version`. This is the number that decides whether the estimation path is doing anything.

---

## 5. Interview preparation

Build the project, then be able to answer these without hesitating. Each maps to something in the design rather than something memorized.

1. Why integer cents rather than floating point, and what specifically goes wrong with floats here.
2. Why replay is a feed implementation rather than a separate backtester, and what that prevents.
3. What happens on a sequence gap and why the response is to stop rather than to repair.
4. Why the fee curve peaks at 50 cents and what that implies about which markets are worth scanning.
5. Why opportunities are ranked by annualized return rather than raw edge, with a concrete example where the ordering flips.
6. How arbitrage detection reduces to linear feasibility over a state space, and where the trade comes from in the dual.
7. Why there are fast paths and an LP fallback rather than one or the other.
8. What the resolution rules trap is and why an automated matcher cannot be trusted alone.
9. Why the system is a single process and how you would know when that stopped being true.
10. What the rejection funnel actually showed, and what that says about whether this edge is real.
11. Why the trading path and the estimation path are different numeric domains, and what goes wrong if a smoothed estimate is fed into a fee comparison.
12. Why Dykstra rather than naive alternating projection, and why a published number that depends on registry file order is indefensible.
13. Whether coherent estimates beat raw midpoints on Brier score and log loss, bucketed by time to resolution, and what that implies for the weight model.

Question ten is the one that separates a project someone built from a project someone described. Question thirteen is the one that separates a coherence claim from a coherence finding.

---

## 6. Cut list

If time runs short, cut in this order. Each cut leaves a coherent project behind.

1. General LP fallback. The four fast paths cover the overwhelming majority of real groups. Document it as future work.
2. Edge versus time scatter chart. The coherence bar alone carries the frontend.
3. Paper executor settlement simulation. Detection without a profit and loss curve is still a complete detection system.
4. Polymarket entirely. Single-venue coherence scanning is a real project and the exhaustive and monotonicity violations are genuinely there.
5. Frontend entirely. Replace with one terminal screenshot and a clear README.
6. Coherence websocket delta stream. The snapshot endpoint plus Parquet is enough to score the study.
7. Coherence engine entirely. Phases 0 through 5 remain a complete project. If you do build it, do not cut the calibration harness or the solver isolation; a projector without scores is an assertion, and an estimate that reaches the solver is a bug.

Do not cut, under any circumstances: sequence gap handling, the freshness gate, the fee model, or the property tests. Those four are what separate this from a script that prints price differences. If the coherence engine is in scope, also do not cut the calibration harness or the rule that estimation never feeds trading.

---

## 7. Immediate next actions

1. Create the repository and push the phase 0 skeleton.
2. Write the fee table test and make it pass.
3. Create a Kalshi account and generate an API key against the demo environment.
4. Subscribe to one public ticker channel using an authenticated connection and print messages to the terminal.

These initial actions are historical and already completed; they do not authorize a new push or commit.

Step four is the first moment the project feels real. Get there in the first week.
