# Phase 4 summary: solver fast paths

Date: September 19, 2026 (UTC). Baseline: `4d8a568` (coherence engine scheduled as
phases 9–10), plus the working tree described here.

Scope: the four closed-form constraint checks, the costing pipeline (freshness gate,
depth walk, size capping, per-level fees, net edge, annualized return), rejection-reason
tracking, and the property tests. The general linear-programming fallback is documented
as future work in `src/solver/mod.rs` and is not implemented. Registry *loading* (phase 5),
the paper executor (phase 6), the real Polymarket fee model (phase 7), and cross-venue
matching (phase 8) stay out.

## 1. Exit criteria

| # | Criterion | Status | Evidence |
|---|---|---|---|
| 1 | All four fast paths run on synthetic and captured fixture input | **Met** | §2.2, §2.4 |
| 2 | Fed 98c rejected; 95c accepted with the expected net edge and correct fees | **Met**: net −158c and +145c | §2.3 |
| 3 | A stale book in any group skips the group | **Met**: `Stale` and `NotLive` distinguished | §2.2 |
| 4 | Depth walk produces correct cost on multi-level books | **Met**: 40-contract trade on sizes [1000, 520, 40, 3000] | §2.2 |
| 5 | Property tests pass: no arbitrage on coherent input; positive payoff in all states | **Met**: 4 property tests | §2.5 |
| 6 | `cargo test`, `cargo clippy -D warnings`, `cargo fmt --check` all pass | **Met** | §2.1 |

Raw output is in [`phase-4-evidence/`](phase-4-evidence/).

## 2. Evidence

### 2.1 Gate

Full output: [`phase-4-evidence/gate.txt`](phase-4-evidence/gate.txt). The commands match CI.

```
$ cargo fmt --all -- --check                  exit=0
$ cargo clippy --all-targets -- -D warnings   exit=0
$ cargo test                                  exit=0
    unittests src/lib.rs   24 passed   (+10: registry 3, solver costing 6, solver fast 4,
                                        minus none; fees and types unchanged)
    tests/book.rs           9 passed
    tests/replay.rs        10 passed
    tests/snapshot_sides.rs 3 passed
    tests/solver.rs        18 passed   (new)
    tests/stage2.rs         8 passed
```

### 2.2 What ships

`src/registry.rs` holds `GroupId`, `Relation`, `ConstraintGroup`, and `Registry` with the
contract-to-group reverse index built at construction. `Relation::resolution_states`
enumerates every resolution a relation permits; that enumeration is the semantic
definition of each relation and is what every signal is priced against.

`src/solver/` is four files. `fast.rs` holds the closed-form checks; `costing.rs` holds
`walk_depth_and_cost`, `apply_fees_per_level`, `min_payoff_per_unit`, `cost_candidate`,
and `rank`; `types.rs` holds candidates, opportunities, `RejectReason`, `SolverConfig`,
and `SolverMetrics`; `mod.rs` holds the dirty dispatch and the `BookSource` trait the
solver reads books and engine time through. `BookStore` implements `BookSource`, taking
time from the same injected clock that stamps `updated_at_ms`, so the freshness gate
decides identically live and under replay.

Supporting changes: `Book::no_asks` / `Book::best_no_ask` (the mirror of `asks`, needed by
every leg that buys the no outcome), `FeeModels` selecting a schedule per venue, and a
zero-fee `PolymarketFees` default.

Covered by `tests/solver.rs`: complement on a crossed book (net +158c at 100 contracts),
exhaustive sets, both ladder cases below, a cross-venue pair against a mock Polymarket
book priced per venue and gated on `verified`, the stale / resyncing / exactly-at-budget
freshness cases, ranking, and depth walking with size capping — sizes
`[1000, 520, 40, 3000]` produce a 40-contract trade whose stepped leg costs
`20×60 + 20×61 = 2420`, not `40×60`, and is charged 34c + 34c rather than one blended
ceiling.

### 2.3 The Fed example

Printed by the real solver, log line included. Full output:
[`phase-4-evidence/fed-example.txt`](phase-4-evidence/fed-example.txt). Reproduce with
`cargo test --test solver -- --exact fed_example_rejection_table --nocapture`.

| Case | Legs | Cost | Fees | Payoff | Net | Outcome |
|---|---|---|---|---|---|---|
| 98c | 4, 62, 29, 3 | 9800 | 358 | 10000 | **−158** | rejected, `fees_exceed_gap` |
| 95c | 3, 60, 29, 3 | 9500 | 355 | 10000 | **+145** | accepted, 17.90% annualized |

```
 INFO sum100::solver: candidate rejected group=0 reason=fees_exceed_gap legs=4
   top_of_book_cost_cents=98 gross_edge_cents=2
```

The 95c case clears the 15% annualized floor only because the fixture resolves in 30 days.
The same trade eight months out returns 2.2% annualized and is rejected as
`below_min_return`. That sensitivity is the ranking layer working, not a fixture artifact.

### 2.4 Live fixture

`live_fixture_book_is_coherent_and_emits_nothing` replays the full `stage2-live-orderbook`
capture through the real parser and `BookStore`, then evaluates a complement group on the
result. The final book quotes yes ask 46 and no ask 56, which sum to 102: coherent, so the
group is evaluated, no candidate is produced, and no rejection is recorded either. This is
the shape of the overwhelming majority of live evaluations.

### 2.5 Property tests

A seeded splitmix64 generator, so any failure replays exactly.

1. **No arbitrage on coherent input.** 500 rounds each of random coherent exhaustive sets,
   uncrossed complements, and monotone ladders. Nothing is emitted.
2. **Payoff guarantee.** 2000 random groups across all four shapes with no coherence
   imposed. For every emitted opportunity, every resolution state the relation permits is
   enumerated and asserted to pay at least the claimed guarantee and to net at least the
   claimed profit after all premium and fees. The run asserts more than 50 opportunities
   were generated, so the property cannot pass vacuously.
3. **Fee behavior under quantity.** Non-decreasing in quantity, subadditive (splitting an
   order never beats batching it), and strictly increasing once the exact fee has moved a
   full cent — across four fee multipliers and all prices 1–99.
4. **Size cap.** 400 rounds of random per-leg depth: the executable quantity equals the
   thinnest leg capped by `max_position_size` whenever every level is profitable, and
   never exceeds it otherwise. Total leg depth, not top-of-book depth, is what counts.

## 3. Where the implementation departs from the written spec

Four places. Each is a deliberate choice, not an oversight.

**Ladder trade direction.** The spec says an inversion is captured by buying "the cheaper
(higher threshold)" and selling "the dearer (lower threshold)". Under its own check
(`price[i] >= price[i+1]` on a ladder ordered by rising threshold), a violation means the
*lower* threshold is the cheaper one, so that parenthetical inverts the trade. The
implementation buys the weaker rung's yes and the stronger rung's no, which pays $1 in
every state and $2 between the thresholds. The reversed position pays nothing when only
the weak rung hits; `min_payoff_per_unit` returns 0 for it and it is refused, which is
asserted directly in `solver::costing::tests::min_payoff_is_computed_from_the_relation_not_assumed`.

**Sizing within the cap.** The spec costs the full thinnest-leg quantity and rejects on a
negative total. Deeper levels cost more while the guaranteed payoff per contract is fixed,
so that rule discards arbitrages whose edge exists only near the top of book — most real
ones. Costing instead takes the profit-maximizing quantity at or below the cap. Because
price per contract is constant inside a level and the per-level fee ceiling only amortizes
as quantity rises, net profit is monotone within a level and can only turn over at a level
boundary, so evaluating the boundaries is exact rather than approximate. When every level
is profitable the answer is the cap, which is the published behavior and what property
test 4 asserts.

**The 96c fixture.** The spec asks for "4 outcomes, sum 96c, fees 3c, net edge +1c". No
four-leg Kalshi trade can be charged 3c at one contract: each level's fee rounds up to at
least a cent on its own, so four legs cost at least 4c and a 4c gap can never clear at
size one. At 100 contracts the same shape lands where the fixture intends — gross 400,
fees 399, net exactly +1 — using prices `[4, 4, 46, 42]`. That case is also rejected under
the shipped config, on `below_min_return` rather than on the arithmetic: one cent on
$99.99 over thirty days annualizes to 0.12%. Both outcomes are asserted. The paired
rejection fixture uses the canonical 98c Fed set rather than the spec's illustrative
"fees 2.5c, net −0.5c", which does not correspond to any real price vector under the
published schedule.

**Fee monotonicity.** The spec's property is that more contracts *strictly* increase total
fees. That is false for a ceiling schedule and asserting it would be asserting a bug: at a
penny price one contract and two contracts both round up to the same cent. The property
test asserts what actually protects the engine — fees never fall, splitting never saves,
and the charge does grow once the exact fee has moved a full cent.

One addition: `Relation::Monotone { ordered }` alongside the documented `Implies`, so an
N-rung ladder is one group evaluated on a single dirty mark rather than N−1 overlapping
pairs. `Implies` dispatches to the same pairwise check as its two-rung case, and
`registry::tests::ladder_states_are_exactly_the_cut_points` asserts the two agree.

## 4. Open

- **Phase 4's own deliverable is not complete.** PLAN.md requires a log of real candidate
  violations from a full replayed trading day with a rejection breakdown, and five
  rejections spot-checked by hand. That needs registry loading (phase 5) to know which
  live contracts form groups, and a CLI path to drive the solver over a replay. The
  detection and costing logic is done and tested; the live-data evidence is not gathered.
- No CLI subcommand exposes the solver yet. `evaluate` is called only from tests.
- Tick-to-signal latency is unmeasured. The fast paths allocate nothing and the costing
  path allocates per candidate, but no p50/p99 numbers exist.
- The general LP fallback remains unimplemented, as scoped.
