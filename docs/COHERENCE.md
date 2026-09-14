# Sum100 Coherence Engine

Version 1.0 — design accepted September 14, 2026.
Status: design accepted, implementation not started.
Depends on: phases 0 through 5 of [PLAN.md](../PLAN.md) (registry and dirty marking).
Cross-venue constraints additionally depend on phase 8 (verified pairs).
Scheduled as phases 9 and 10, before the frontend, which should display it.

This is the first component in the project that is genuinely novel rather than
well-trodden. The trading path is a careful implementation of known arbitrage
detection. The estimation path publishes a number nobody else computes: the
nearest internally consistent probability vector to the live books, under a
depth-aware metric. The two paths share a book store and otherwise do not know
each other exist.

---

## 1. Purpose

Every published prediction market probability is a raw quote from a single venue. That number has three problems. It is frequently stale, since a last trade price can be hours old. It is frequently incoherent with its own sibling contracts, since a set of mutually exclusive outcomes routinely fails to sum to one dollar. And it ignores depth entirely, so a price backed by forty contracts is presented with the same authority as one backed by six thousand.

The coherence engine publishes the corrected version. It takes the observed quotes across both venues, finds the nearest set of probabilities that satisfies every logical constraint the registry knows about, weights the correction by how much real liquidity stands behind each quote, and streams the result.

Two properties define the output.

**Internal consistency by construction.** No two numbers the engine publishes can contradict each other. Every exhaustive set sums to one, every ladder is monotone, and every verified cross-venue pair agrees. This is a guarantee, not a best effort.

**No introduced view.** The engine never predicts anything. The output is the minimum-distance correction to the market's own numbers under a depth-aware metric. Where the market is already coherent, the output is the input, exactly.

That second property is what makes this a data product rather than a model. The engine has no opinion about whether the Fed will cut rates. It only removes arithmetic contradictions from numbers other people already rely on.

Neither venue can produce this, because each sees only its own book.

---

## 2. The two numeric domains

This is the boundary everything else follows from. Sum100 now serves two purposes with incompatible numeric requirements. Mixing them is the failure mode that would make the trading path indefensible.

| | Trading path | Estimation path |
|---|---|---|
| Question | Is this trade profitable, yes or no | What is the coherent probability |
| Representation | `i64` integer cents | `f64` probability in 0.0 to 1.0 |
| Floating point | Banned | Required |
| Division, averaging, iteration | Never | Inherent |
| Rounding | Exact, conservative direction | At publication only, once |
| Precision | A cent is the unit of money | 62.01 percent is a real estimate and is not expressible in cents |
| Consequence of error | A losing trade | A slightly wrong published number |

The trading path answers a go or no-go question against an exact threshold. Integer cents, never floats. A rounding error is a bug that costs money. This is ADR-005 in the architecture document, unchanged.

The estimation path produces a statistical quantity. It needs fractional precision, and it involves division, averaging, and iteration. Floats are correct here.

**Hard rule.** No projector output may ever reach the solver, and no solver decision may depend on an estimate. The two paths read the same book store and the same dirty set, and are otherwise unaware of each other.

The failure mode this prevents is specific and easy to fall into. Someone later notices the coherent estimate is smoother than the raw midpoint and feeds it into a fee comparison to reduce noise. At that moment the arbitrage detector is trading on a number the system invented rather than a number a venue quoted, and every signal it emits becomes indefensible. The violation would look like an improvement. That is why the rule is structural rather than a comment.

Enforce it in the module graph. The projector lives in `src/coherence/`. Its output types are not imported from `solver`. No function in the solver takes a `Quote` or a `CoherentEstimate`. Duplicated quote-extraction logic between the two paths is deliberate.

---

## 3. Position in the pipeline

```
                        +-------------+
                        | book store  |
                        +------+------+
                               |
             +-----------------+-----------------+
             |                                   |
      +------v------+                     +------v-------+
      |   solver    |                     |    quote     |
      | violations  |                     |  summarizer  |
      +------+------+                     +------+-------+
             |                                   |
      +------v------+                     +------v-------+
      |   paper     |                     |  projector   |
      |  executor   |                     |              |
      +-------------+                     +------+-------+
                                                 |
                                          +------v-------+
                                          |  publisher   |
                                          +--------------+
     exact integer cents                   statistical, f64
```

Both branches consume the dirty set produced by the book store. The registry feeds both, since both need to know which contracts relate to which. Dirty marking promotes a contract to its cluster (estimation) and to its constraint groups (trading). The two promotions are independent.

The branches run in sequence on the engine task, solver first. The solver's work is measured in microseconds and its latency budget is the tighter of the two, so it does not wait behind projection work. If projection ever grows expensive enough to matter, it moves to its own task fed by a coalescing channel, but not before measurement says so.

The unit of estimation work is a cluster, not a group. See section 7.

---

## 4. Data model

### 4.1 Quote

The summarizer's output. One per contract with a usable book. Turning a book into one number plus a confidence is where most of the judgment lives, and it is the part that will be wrong first.

```rust
pub struct Quote {
    pub contract: ContractId,
    pub p: f64,                 // point estimate, 0.0 to 1.0
    pub weight: f64,            // confidence, non-negative, unbounded above
    pub depth: i64,             // contracts near the mid, for reporting
    pub spread_cents: i64,
    pub ts_ms: u64,
    pub quality: QuoteQuality,
}

pub enum QuoteQuality {
    TwoSided,     // real bid and real ask, the normal case
    OneSided,     // only one side quoted
    Crossed,      // bid above ask, venue glitch or mid-update artifact
    NoQuote,      // empty book; this quote is excluded from the group
}
```

`p` is only a midpoint for `TwoSided`. The other three variants do not fabricate one. See section 5.3.

### 4.2 Cluster

The unit of projection work. A connected component of the constraint graph.

```rust
pub struct Cluster {
    pub id: ClusterId,
    pub members: Vec<ContractId>,      // sorted, for determinism
    pub groups: Vec<GroupId>,          // constraints spanning these members
}
```

Clusters are computed once at registry load and never change at runtime. Most contain a single group of four to twelve contracts. A cluster with two hundred members indicates a registry modeling error rather than a hard math problem. The loader logs the size distribution so that shows up immediately, and it refuses to start if any cluster exceeds a hard cap.

### 4.3 Projection result

```rust
pub struct Projector {
    pub max_iters: usize,     // hard cap, 50 by default
    pub tolerance: f64,       // max acceptable residual, 1e-9 by default
}

pub struct Projection {
    pub cluster: ClusterId,
    pub estimates: Vec<(ContractId, f64)>,  // sorted by ContractId
    pub iters: usize,
    pub residual: f64,          // max constraint violation remaining
    pub converged: bool,
}
```

Cap the iterations and publish residual and convergence as fields. Never loop unbounded on a hot path. If a cluster fails to converge, publish the raw quotes with `converged: false` rather than a corrected number you cannot defend.

### 4.4 Published estimate

```rust
pub struct CoherentEstimate {
    pub event: EventId,
    pub outcome: ContractId,
    pub p: f64,
    pub shift: f64,             // p minus the raw midpoint, 0.0 if none
    pub weight: f64,
    pub depth: i64,
    pub venues: Vec<Venue>,     // which venues contributed
    pub quality: QuoteQuality,
    pub status: EstimateStatus,
    pub residual: f64,
    pub as_of_ms: u64,
    pub model_version: u32,
}

pub enum EstimateStatus {
    Coherent,            // projected, residual below tolerance
    Incomplete,          // a member was excluded; constraint weakened
    Degraded,            // a member is resyncing; not projected
    NotConverged,        // hit the iteration cap; p is the raw quote
    UniformFallback,     // all weights were zero; projected under uniform weights
}
```

`shift` is a product feature, not a diagnostic. A large shift means the market currently contradicts itself about that event. No venue reports this and no aggregator computes it.

`model_version` increments whenever the weight model or projection method changes. Anyone consuming a time series needs to know where the methodology moved, and without it your own calibration study will silently compare across a discontinuity.

---

## 5. Quote summarization

This is where the judgment lives and where the first version will be wrong.

### 5.1 Point estimate

The midpoint of best bid and best ask, expressed in probability units: `((best_bid_cents + best_ask_cents) / 2) / 100`. Not last trade, which can be hours stale on a thin market and reflects a moment that has passed. Midpoint is a statement about the current book.

### 5.2 Weight

Weight expresses how much the correction should resist moving this quote. It rises with depth, falls with spread, and falls with staleness.

```rust
pub trait WeightModel {
    fn weight(&self, book: &Book, now_ms: u64) -> f64;
}
```

The exact formula sits behind this trait so it can be swapped and A/B tested against the calibration study in section 13 rather than argued about.

The initial implementation has the shape of depth available within a few cents of the mid, divided by one plus the spread in cents, multiplied by an exponential decay in quote age:

```text
w = depth_near_mid / (1 + spread_cents) * exp(-age_ms / tau)
```

`depth_near_mid` is the sum of size at levels within a small window of the midpoint, not the entire book. A resting order twenty cents from the mid is not confidence in the current price. `tau` is a half-life in milliseconds; start with something on the order of tens of seconds and let calibration move it.

Two properties the model must have regardless of form:

1. **Monotone increasing in depth.** More liquidity can never mean less confidence.
2. **Bounded below at zero.** A negative weight would invert the projection, pulling the result toward the thin quotes instead of away from them.

The constants are not a design argument. They are an empirical question the calibration harness answers.

### 5.3 Quality cases

The edge cases determine whether the output is trustworthy on illiquid events, which is most events. Handle them explicitly or you will quietly publish garbage.

**TwoSided.** Real bid and real ask. Midpoint is well-defined. This is the normal path.

**OneSided.** Only one side is quoted, so no midpoint exists. Do not fabricate one from the single side. Emit the quoted side as a bound (`p` set to the bid, or to `1 - ask/100` if only the ask is present) with a very small weight, so the projection is free to move it almost anywhere the constraints require. The implied interval is wide. A fabricated midpoint on a one-sided book is a number you invented and then treated as evidence.

**Crossed.** The bid sits above the ask. This is a venue glitch or a book caught mid-update. Keep the member in the set so the constraint stays complete, but drop its weight to near zero and flag `quality: Crossed`. The projector still sees the contract; it just refuses to believe the price.

**NoQuote.** Empty book. Exclude the member entirely. This changes the constraint itself. An exhaustive set missing a member no longer has to sum to one, and projecting as though it does would push the remaining prices to absorb probability mass belonging to a contract nobody is quoting. Mark the group incomplete (`EstimateStatus::Incomplete`) and publish the survivors rather than a fabricated correction.

That last case is the single most likely source of embarrassingly wrong published numbers. It is handled by refusing rather than by guessing. An incomplete exhaustive set is a different mathematical object from a complete one. Treating it as complete is a silent modeling error.

---

## 6. Constraint projection

Three constraint families, three algorithms, all exact and all linear in group size. These are the primitive projections. Dykstra (section 7) composes them when a contract sits in more than one family at once.

### 6.1 Exhaustive set

Members are mutually exclusive and jointly exhaustive, so their probabilities must sum to one.

The correction is a weighted projection onto the probability simplex. The unweighted version spreads the discrepancy evenly across members, which is wrong: it nudges a price that six thousand contracts agree on by the same amount as a price backed by one stale resting order. The weighted version distributes the discrepancy inversely to weight, so thin quotes absorb nearly all of it and deep quotes barely move.

On the equality `1ᵀp = 1`, ignoring the box constraints for a moment, the minimizer of `½ Σᵢ wᵢ (pᵢ − qᵢ)²` is

```text
pᵢ = qᵢ − λ / wᵢ
λ  = (Σᵢ qᵢ − 1) / Σᵢ (1 / wᵢ)
```

Worked example. Three outcomes quoted at 0.40, 0.40, 0.40. Weights 100, 100, 1. The quotes sum to 1.20. Then `λ = 0.20 / (0.01 + 0.01 + 1) ≈ 0.196`, and the projected values are approximately 0.398, 0.398, 0.204. The thin quote absorbed nearly the entire 0.20 excess. Uniform weighting would have moved each quote by 0.067, including the two that the book is confident about.

After the hyperplane step, clip each coordinate into `[0, 1]` and, if clipping broke the sum, rebalance over the unclipped coordinates. The loop is finite: each clip permanently removes a coordinate, so it runs at most once per member. Closed form plus a linear cleanup. No iteration tolerance.

When the group is incomplete because a member was excluded (`NoQuote`), do not project onto `Σ pᵢ = 1`. The remaining members are still mutually exclusive, so they satisfy the weaker constraint `Σ pᵢ ≤ 1`. Whether to enforce that inequality or to skip the group is an open question (section 16.2). The current default is to skip and flag `Incomplete`.

### 6.2 Monotone ladder

For threshold markets ordered by strike, probabilities must be non-increasing as the threshold rises, because a higher bar is strictly harder to clear. If "Fed cuts by 50bp" is 40 percent, "Fed cuts by 25bp or more" cannot be 30 percent.

This is weighted isotonic regression, solved exactly by pool adjacent violators (PAV). Scan the ladder once in strike order. Whenever two adjacent members violate the ordering, merge them into a block holding their weighted average

```text
p_block = (wₐ pₐ + wᵦ pᵦ) / (wₐ + wᵦ)
```

then check backward in case the merge broke the block before it. Each element is merged at most once, so the whole pass is linear in ladder length.

No convergence criterion, no tolerance, no iteration count. The algorithm terminates with the exact answer. The result is the nearest non-increasing sequence in the weighted Euclidean metric, which is the same metric the simplex step uses, which is why Dykstra can compose them.

### 6.3 Cross-venue equivalence

For a verified pair, both venues are estimating the same quantity. Combine them as a weighted average, so the venue with real depth in that particular market dominates:

```text
p = (wₐ qₐ + wᵦ qᵦ) / (wₐ + wᵦ)
```

Only verified pairs participate. This is the same gate the trading path uses, and for the same reason: two contracts with similar titles and different resolution rules are not the same event, and averaging them publishes a number that describes neither.

Unverified candidates do not enter the constraint graph, so they cannot join a cluster, so they cannot pull an estimate. The verification workflow in the architecture document is load-bearing for this path too.

### 6.4 Why the three cannot simply be applied in turn

A contract routinely belongs to several groups at once. A ladder rung is also a member of an exhaustive set and also has a verified Polymarket twin.

Projecting onto each constraint set in sequence does not land in the intersection. It approaches it while oscillating, and where it settles depends on the order the projections were applied, which in turn would depend on the order groups appear in the registry file. Naive alternating projection (POCS) reaches some feasible point, not the nearest one. A published number that changes when someone reorders a configuration file is indefensible.

---

## 7. Clusters and Dykstra's algorithm

### 7.1 The constraint graph

Nodes are contracts. Two nodes share an edge when they co-occur in any constraint group. A cluster is a connected component of that graph.

Compute connected components once at registry load, iterating members in sorted `ContractId` order so the component labeling is deterministic. Store, for each contract, its `ClusterId`. Dirty marking then promotes a contract to its cluster in a single lookup: one book update dirties one cluster, not one group.

Log the size distribution at load. Expected shape: a large majority of clusters are a single group of four to twelve contracts; a handful are a ladder plus its exhaustive parent plus a few cross-venue twins; none should be large. A cluster of two hundred members is a registry modeling mistake, almost always an accidental cross-edge that glued unrelated events together. The loader emits a loud error above a configurable threshold (start at 64) and refuses to start above a hard cap (start at 200). That is not a math problem and must not be treated as one.

### 7.2 Dykstra

The correct tool is Dykstra's algorithm: alternating projections with a per-set increment that makes the limit the true projection onto the intersection rather than merely some feasible point.

All three constraint sets are convex. The simplex is convex, the monotone cone is convex, and an equality between two coordinates is convex. Finite intersections of convex sets are convex. Dykstra therefore converges, and the limit is independent of projection order.

```text
Iᵢ ← 0 for each constraint Cᵢ in the cluster
x  ← raw quotes, in sorted ContractId order

for k in 1..=max_iters:
    for each Cᵢ in fixed (sorted GroupId) order:
        y  ← x − Iᵢ
        x  ← P_{Cᵢ}(y)          // the primitive from section 6
        Iᵢ ← x − y              // the correction term
    residual ← max constraint violation of x
    if residual < tolerance: break
```

`Iᵢ` is the increment that naive alternation omits. Without it, the composition depends on order. With it, the limit is the projection onto the intersection in the same weighted Euclidean metric each primitive uses.

```rust
impl Projector {
    pub fn project(&self, cluster: &Cluster, quotes: &[Quote]) -> Projection;
}
```

Iteration stops on whichever comes first: residual below tolerance, or the iteration cap. There is no unbounded loop anywhere on a live path. Defaults: `max_iters = 50`, `tolerance = 1e-9`.

When a cluster hits the cap without converging, publish the raw quotes with `EstimateStatus::NotConverged`. Do not publish a partially corrected number as though it were final. A consumer can handle a flagged value. It cannot handle a confident wrong one.

In practice the overwhelming majority of clusters are a single group, which means one primitive projection and no Dykstra iteration at all. The machinery exists for the minority of clusters that need it and costs nothing on the common path. Short-circuit it: if the cluster has one group, call that group's primitive and return.

### 7.3 Input already feasible

If every constraint in the cluster is already satisfied to within tolerance, the projection is the identity. Return the input quotes, `iters = 0`, `converged = true`. This is both the cheap path and the defining property of a projection: the projection of a feasible point is that point. Property tests assert it exactly, not approximately, on already-coherent inputs.

---

## 8. Numerics and determinism

Floating point is correct for this path, but replay determinism is still a requirement. The regression corpus and the calibration study both depend on reproducible output. Byte-identical replay is already a project invariant (architecture, non-functional goals). The estimation path does not get a weaker standard.

**Fixed iteration order.** Iterate clusters in sorted `ClusterId` order and members in sorted `ContractId` order. Constraint groups inside a cluster iterate in sorted `GroupId` order. Never iterate a `HashMap` directly. Rust randomizes hash seeds per process, so map iteration order differs between runs, and floating point addition is not associative. That combination alone would make replay output differ run to run.

**No parallelism without a measured reason.** Do not parallelize the projector across clusters until a benchmark says the engine task is saturated by projection. A rayon parallel reduce would produce slightly different sums per run for the same input, which destroys the byte-identical replay property. Projection work is microseconds per cluster on the common path. There is nothing to gain here.

**Round once, at the publication boundary.** Four decimal places. Every internal value stays full precision. Rounding inside the loop would accumulate and would make idempotence fail.

**Guard the degenerate cases.**

| Case | Response |
|---|---|
| All weights zero | Fall back to uniform weighting, `EstimateStatus::UniformFallback` |
| Single-member cluster | No-op, return the quote |
| Empty cluster | Registry bug, fail at load |
| Cluster larger than the log threshold | Log loudly; this is a registry error |
| Cluster larger than the hard cap | Refuse to start |

---

## 9. Publication

### 9.1 Endpoints

```
GET  /api/coherent                     snapshot of all current estimates
GET  /api/coherent/{event_id}          one event
GET  /api/coherent/{event_id}/history  time series for charting
GET  /ws/coherent                      delta stream of changed events
```

The API performs no calculation, in line with ADR-006 in the architecture document. Every field it serializes was computed upstream.

### 9.2 Cadence

Publish on a fixed cadence of roughly ten updates per second, coalescing per event, rather than on every tick. Consumers do not want microsecond resolution and the engine should not spend its time serializing at tick rate.

Coalescing is allowed here because a coherent estimate is a complete derived view, not a signed delta. The architecture document forbids coalescing raw book deltas; that prohibition does not apply to this stream. The coalescing buffer holds the latest estimate per event and flushes on the cadence timer.

### 9.3 Contract with consumers

Every response carries `model_version`, `as_of_ms`, `status`, `shift`, and `residual`. A consumer that caches must be able to tell how old a number is, whether the method behind it changed, and whether it is a projection or a flagged fallback.

`shift` is part of the payload, not an optional debug field. A consumer that wants "the market currently disagrees with itself by X" should not have to recompute it.

---

## 10. Storage

Append published estimates to Parquet, partitioned by UTC day. Column schema matches `CoherentEstimate` plus the raw midpoint so the calibration study can score both series without joining back to the book log.

Writing one row per event per publication tick is far too much volume for what it buys. Write on change beyond a threshold, plus a heartbeat row every few seconds so gaps in the series are unambiguous:

- Write if `|p_now − p_last_written|` exceeds a small threshold (start at `1e-4`, the publication quantum).
- Otherwise write a heartbeat if more than a few seconds have passed since the last row for that event.
- Never skip a status change. A move from `Coherent` to `Degraded` is always written.

This history is not optional infrastructure. The calibration study in section 13 is only possible if the estimate time series exists for events that have since resolved, and that history cannot be reconstructed after the fact. Retrofitting storage after the projector exists means waiting weeks for new events to resolve. Build it in the same phase.

Use the injected clock for `as_of_ms`, the same clock replay already uses. Storage paths under live versus replay must not diverge in schema, only in destination directory.

---

## 11. Failure modes

The pattern throughout is to degrade and label rather than to guess or to go silent. A consumer can handle a flagged number. It cannot handle a confident wrong one.

| Failure | Detection | Response |
|---|---|---|
| Any member book is `Resyncing` or `Uninitialized` | `BookState` on a cluster member | Do not project. Publish the cluster as `Degraded`. Leave last coherent values in storage with a heartbeat so the gap is visible |
| Crossed book | `best_bid > best_ask` | Flag `QuoteQuality::Crossed`, drop weight to near zero, keep the member in the set |
| One-sided book | Only one side has depth | Wide interval, tiny weight, `p` is the quoted side, no fabricated midpoint |
| Empty book | Both sides empty | Exclude the member. Mark the exhaustive constraint incomplete. Publish survivors as `Incomplete` |
| Cluster fails to converge | Residual still above tolerance at `max_iters` | Publish raw quotes, `NotConverged`, include the residual |
| Cluster larger than the log threshold | Component size at registry load | Log loudly. This is a registry error, not a math problem |
| Cluster larger than the hard cap | Component size at registry load | Refuse to start |
| All weights zero | Sum of member weights is 0 | Uniform weighting, `UniformFallback` |
| Input already feasible | Max violation below tolerance before any iteration | Identity. `iters = 0`, `Coherent` |
| Unverified cross-venue pair | `verified == false` on the binding | The pair is not in the constraint graph. No averaging, no cluster edge |
| Negative-risk Polymarket event | Registry flag, inherited from the solver | Exclude from the projector until modeled. Same exclusion the exhaustive fast path needs |

Books that are not `Live` never contribute a quote. The trading path already refuses to evaluate a group containing a non-live book; the estimation path refuses to project a cluster containing one. The responses differ in kind (no signal versus a flagged estimate) and agree in refusing to invent a number.

---

## 12. Observability

Metrics collected continuously, exposed on the health endpoint, and later on the frontend health strip.

**Coverage.** Events with a coherent estimate. Events in `Incomplete`. Events in `Degraded`. Events with no usable quote. Fraction of the universe in each status.

**Correction magnitude.** Distribution of `shift`. Mean absolute shift by venue and by event category. Events where `|shift|` exceeded a threshold, which is the incoherence signal itself.

**Convergence.** Clusters converged. Mean, p50, and p99 iteration count. Clusters that hit the cap. Residual histogram for converged and for not-converged clusters separately.

**Cluster shape.** Size histogram, logged at registry load and on any reload. Count of clusters above the log threshold.

**Latency.** Time from dirty mark to published estimate, at p50 and p99. This is allowed to be slower than tick-to-signal. It is not allowed to be unbounded.

**Calibration, once events resolve.** Brier score and log loss for coherent and for raw, by bucket of days to resolution. These are the numbers that decide whether the engine is doing anything.

---

## 13. Validation and calibration

Property tests first. Then the comparison that turns this from an assertion into a finding.

### 13.1 Property tests

These assert correctness of the projection itself and should be written alongside it, not after. They are cheap and they catch the silent failures.

- Output satisfies every constraint in the cluster, to within tolerance.
- Projection is idempotent. Projecting twice equals projecting once, to within rounding at the publication boundary. Internally, `project(project(x)) = project(x)` exactly on the pre-rounded values.
- When the input already satisfies every constraint, output equals input exactly. No drift on coherent markets.
- Every output probability lies in zero to one.
- Output is independent of the order groups appear within the cluster. Shuffle the group list and assert identical results. This is the property Dykstra exists to provide, so test it directly.
- Increasing one member's weight strictly decreases how far that member moves, all else equal.
- An excluded `NoQuote` member does not appear in the output, and the remaining exhaustive members are not forced to sum to one.
- Replay of a recorded session produces byte-identical published estimates, including `status`, `shift`, and `residual`.

### 13.2 The calibration study

This is what turns the coherence engine from an assertion into a finding, and it is the single strongest thing this project could produce.

For every event that resolves, two time series exist: the raw midpoint history and the coherent estimate history. Score both against the realized outcome.

**Brier score.** `(p − y)²` where `y ∈ {0, 1}`. Proper, bounded, interpretable.

**Log loss.** `−(y log p + (1 − y) log(1 − p))`, with `p` clipped into `(ε, 1 − ε)` so a published zero does not produce infinities. Harsher on confident mistakes, which is the failure mode this engine exists to avoid.

Bucket by time to resolution so a number published thirty days out is not averaged with a number published thirty minutes out. Start with buckets of under one day, one to seven days, seven to thirty days, and beyond thirty.

The question being answered is direct. Does enforcing internal consistency produce better forecasts than the raw prices it started from?

If coherent beats raw, there is empirical evidence that consistency improves forecasting. That is a publishable result. If it does not, that is also a real finding, and it points at the weight model as the thing to fix rather than at the idea of projection.

Either outcome is reportable. What matters is that the claim is measured rather than asserted. The project stops saying the number is better and starts showing the score.

Build the harness in the same phase as the projector. Retrofitting it means waiting weeks for new events to resolve, because the history you need was never written.

The same harness answers the cross-venue question in section 16.3: does the merged estimate calibrate better than either venue alone? That comparison is three series, not two, and it is only meaningful on verified pairs.

### 13.3 Scoring hygiene

Score only estimates whose `status` is `Coherent`. Mixing `Degraded` and `NotConverged` into the headline number hides whether the projector helps. Report those series separately.

Slice by `model_version`. A weight-formula change is a new method. Pooling across versions is how a backtest lies.

Do not score against last trade. The baseline is the raw midpoint, because that is the input. Beating last trade would be a weaker and less honest claim.

---

## 14. Decision records

### ADR-101: Projection rather than a model

**Status.** Accepted.

**Context.** The engine could improve published probabilities either by correcting them toward consistency or by modeling them directly with additional information.

**Decision.** Projection only. The engine introduces no view of its own.

**Reasoning.** A model requires defending a forecast. A projection only requires defending arithmetic. The output is the minimum-distance correction under a stated metric, which means every number can be traced to quotes the venues published plus a published weight formula. It is also the only version that is correct by construction on markets that are already coherent, since the projection of a feasible point is that point.

**Consequences.** The engine cannot improve on a market that is internally consistent but wrong. That is the correct limitation to accept, and it is what keeps the product defensible. Temporal smoothing, if ever added, must be a separate published field, not a modification of the coherent estimate.

### ADR-102: Depth-weighted rather than uniform correction

**Status.** Accepted.

**Context.** A discrepancy has to be distributed across members somehow.

**Options.** Uniform distribution; distribution inversely proportional to a confidence weight.

**Decision.** Weighted, with the weight model behind a trait.

**Reasoning.** Uniform correction moves a price backed by six thousand contracts by the same amount as one backed by twenty. That is not a neutral choice, it is a wrong one, and it would degrade the exact numbers users care most about. Putting the weight model behind a trait means the constants get decided by the calibration study rather than by argument.

**Consequences.** The weight model becomes a load-bearing component that needs its own versioning (`model_version`) and its own evaluation. The initial formula is a starting point, not a result.

### ADR-103: Dykstra rather than naive alternating projection

**Status.** Accepted.

**Context.** Contracts belong to multiple overlapping constraint groups.

**Decision.** Dykstra's algorithm over connected components of the constraint graph.

**Reasoning.** Naive alternating projection reaches the intersection but not the nearest point in it, and where it lands depends on projection order. Since projection order would follow registry file ordering, published numbers would change when someone reorders a configuration file. Dykstra's correction terms make the limit order-independent, which is testable and is tested.

**Consequences.** An iterative algorithm on a live path, which requires an iteration cap, a residual, and a convergence flag in the published output. Single-group clusters skip the loop entirely.

### ADR-104: Bounded iteration with a published residual

**Status.** Accepted.

**Context.** Iterative algorithms in real-time systems cannot loop until satisfied.

**Decision.** Hard iteration cap. Publish residual and `EstimateStatus` as fields.

**Reasoning.** A cluster that fails to converge is information, not an error to hide. Publishing the raw quotes with a clear flag lets consumers decide. Silently publishing a half-corrected number does not.

**Consequences.** Consumers must handle a not-converged case. This is documented in the API contract rather than hidden behind a fallback.

### ADR-105: Estimation output never reaches the trading path

**Status.** Accepted.

**Context.** The coherent estimate is smoother and more stable than a raw midpoint, which makes it tempting as a solver input.

**Decision.** Structurally prohibited. Separate modules, non-importable types, no shared function signatures.

**Reasoning.** The solver's entire defensibility rests on every input being a price a venue actually quoted. Feeding it a computed estimate would mean emitting arbitrage signals against numbers the system invented. The violation would look like a noise reduction improvement, which is exactly why it needs a structural barrier rather than a convention.

**Consequences.** Some duplicated quote extraction logic between the two paths. That duplication is deliberate.

### ADR-106: Publish on a cadence, store on change

**Status.** Accepted.

**Context.** Tick-rate serialization and tick-rate Parquet writes are both more volume than the product needs.

**Decision.** Websocket and snapshot endpoints flush at about ten hertz, coalesced per event. Parquet writes on material change plus a short heartbeat, and always on a status change.

**Reasoning.** Consumers do not want microsecond updates. The calibration study needs a complete history with unambiguous gaps, not a row per tick. Coalescing is valid because the payload is a complete derived view.

**Consequences.** Two timestamps exist: `as_of_ms` is the book time of the estimate, and the row's write time is when it hit disk. Score calibration against `as_of_ms`.

---

## 15. Build order

This slots in after phase 5, because it needs the registry, and before the frontend, because the frontend should display it. Cross-venue primitives additionally need phase 8's verified pairs. The schedule in [PLAN.md](../PLAN.md) therefore places the work as phases 9 and 10, after matching and before the UI. Steps 1 through 3 can start as soon as phase 5 lands, against single-venue groups, if time allows.

1. **Quote summarization** with the `WeightModel` trait and all four quality cases. Property tests for midpoint, one-sided refusal to fabricate, crossed near-zero weight, and empty exclusion.
2. **Simplex projection** with property tests. Demonstrable on a single exhaustive event. This is a weekend with step 3.
3. **Isotonic regression** with property tests. Demonstrable on a single ladder. After steps 1 through 3, single-group events already produce something you can screenshot.
4. **Cluster construction** as connected components of the constraint graph, computed at registry load, with size logging and the hard cap.
5. **Dykstra over clusters**, with iteration caps, residual reporting, the single-group short circuit, and the shuffle-order property test. This is the real engineering.
6. **Publication types, endpoints, and storage.** Snapshot plus websocket. Parquet on change plus heartbeat. `model_version` from day one.
7. **Calibration harness** and the first scored comparison of coherent versus raw, bucketed by time to resolution. Build this in the same stretch as step 6, not later.

Steps 1 through 3 already produce something demonstrable on single-group events. Steps 4 and 5 are the real engineering. Step 7 is what you talk about in interviews.

Module layout:

```
src/coherence/
    mod.rs          // public types, no solver imports
    quote.rs        // Quote, QuoteQuality, WeightModel
    simplex.rs      // exhaustive primitive
    isotonic.rs     // PAV primitive
    cluster.rs      // connected components at registry load
    dykstra.rs      // Projector
    publish.rs      // CoherentEstimate, status, rounding
```

`solver/` does not import this tree. That is the ADR-105 barrier as a compiler rule.

---

## 16. Open questions

1. **Weight model form.** The initial shape is a starting point, not a result. The calibration study should decide between depth-only, depth over spread, and a recency-weighted variant. Until then every published number carries a `model_version` so the change is traceable.

2. **Incomplete exhaustive sets.** Currently these are degraded and flagged. An alternative is to project onto the weaker constraint that the observed members sum to at most one, which is still informative and still correct. This is probably the better answer but has not been worked through.

3. **Cross-venue disagreement that is real.** Two venues can legitimately disagree if their user bases hold different information. Averaging them assumes disagreement is noise. Measuring whether the merged estimate calibrates better than either venue alone would settle it, and the calibration harness can answer this directly.

4. **Temporal smoothing.** The current design is memoryless, so each publication depends only on the current books. Adding a time-based prior would reduce jitter on thin markets but introduces a view, which conflicts with ADR-101. If it is added, it must be a separate published field rather than a modification to the coherent estimate.

5. **Negative risk markets.** As noted in the architecture document's open questions, certain Polymarket multi-outcome events allow converting no positions across outcomes, which changes what the exhaustive constraint means. The projector inherits this problem from the solver and must exclude those markets until it is modeled.
