# Sum100

Prediction markets routinely disagree with themselves. Four mutually exclusive outcomes trade for 96 cents on the dollar. A threshold ladder inverts. The same event prices differently on two venues. Sum100 reads those books live, in Rust, and prices every gap it finds until almost all of them die.

Fees kill most of them. That's the interesting part.

Paper trading by default. It *can* place real Kalshi orders — four separate gates stand in the way, and forgetting any of them leaves you in simulation.

**Runs today:** authenticated Kalshi feed and raw recorder, book store with subscription-scoped gap detection and resync, byte-exact offline replay, the solver — four closed-form constraint checks, depth- and fee-aware costing, ranked by annualized return — the engine task that loads a constraint graph from TOML, marks groups dirty and solves them, and the execution layer: fill-or-kill multi-leg orders, a portfolio with mark-to-market and a daily loss limit, venue health, and position limits.
**Doesn't yet:** unwind a legged position, Polymarket, an HTTP API, the coherence engine ([docs/COHERENCE.md](docs/COHERENCE.md)), UI.

Design is [ARCHITECTURE.md](ARCHITECTURE.md), schedule is [PLAN.md](PLAN.md). This file is the only source for what actually runs. [README.reference.md](README.reference.md) is the original import under the old name Parity, kept verbatim, historical only.

## Run it

```sh
cargo build --release
```

Credentials go in `~/.zshenv`, outside the repo:

```sh
export KALSHI_KEY_ID='your-key-id'
export KALSHI_PRIVATE_KEY_PATH="$HOME/.config/kalshi/key.pem"
```

`chmod 600` the key. PKCS#1 and PKCS#8 both load; anything missing or malformed fails at startup rather than at 3am. Non-interactive zsh reads `~/.zshenv` and ignores `~/.zshrc`, so move the exports rather than keeping two copies. Every Kalshi socket is signed, public channels included, and demo keys do not authenticate against production.

Demo is the default everywhere. Production is `--prod`, and it logs a warning when you pick it. Discovery is `GET /events` and `GET /markets`; nothing in this binary can place, cancel, or transfer.

```sh
make registry-validate                                        # load the graph, no network
make scan-replay FILE=data/phase2-1-e/production/kalshi-2026-09-14.ndjson.gz
make scan-prod SECONDS=70                                     # live; tickers come from the registry
make trade-replay FILE=... CAPITAL=500000                     # paper fills, nothing placed
make trade-paper-prod SECONDS=70                              # live feed, simulated fills

make record TICKERS=YOUR-DEMO-TICKER                          # record demo
make dump-prod TICKERS=KXBTCD-26SEP1417-T76999.99 SECONDS=70  # live bid/ask table
make dump-digest TICKERS=KXBTCD-26SEP1417-T76999.99 SECONDS=180
make replay FILE=data/production/kalshi-2026-09-14.ndjson.gz  # verified against that run
make probe-prod TICKER=KXBTCD-26SEP1417-T76999.99             # raw payloads
make markets-prod SERIES=KXBTCD
make check                                                    # fmt, clippy -D warnings, test
```

The Makefile is sugar over `cargo run --release --`; `make help` lists the knobs (`TICKERS`, `SECONDS`, `FILE`, `DIGEST`, `SESSION`, `PACE`, `EXTRA`). `src/main.rs` is the real flag surface. `--tickers` is required so a missing list can't quietly subscribe to an expired market, and the dated ticker above is a capture artifact — find a live one first. `RUST_LOG=debug` for normalized events; `info` is the default, with metrics every 30 seconds.

## The pipeline

Feed → book store → solver. One direction, no calls backwards. That's what makes replay exact and concurrency a non-issue.

**Feed** (`src/feed/`). One trait: `Feed::next() -> Option<FeedEvent>`. Above it is venue trivia, below it is deterministic logic. Kalshi and replay implement it. Snapshots carry both resting sides at venue prices; a side Kalshi omits — common on far strikes — is applied empty rather than treated as an error. Prices go from decimal strings straight to integer cents, and sub-cent precision is rejected instead of rounded, because a tick-size change should surface loudly rather than cost a cent six weeks later. Sizes floor to whole contracts: understating depth is the safe direction. Reconnect backs off 250 ms to 30 s with jitter and emits `Resubscribed` only once the venue acknowledges.

**Book store** (`src/book.rs`). Dense `[i64; 101]` arrays per side, so a delta is one index and memory is bounded by construction. Yes asks are the resting no bids read at `100 - P`. Snapshots are absolute and always applied. Sequence numbers are scoped to the subscription, not the contract, so a single gap marks every live book `Resyncing` and the driver drops the socket to force a fresh snapshot through the existing reconnect path. No repair, no interpolation: a wrong book costs money, a brief blind spot costs one opportunity out of thousands.

A crossed book (`yes bid + no bid > 100`) stays `Live` and gets counted. That's the complement arbitrage, sitting in plain sight.

**Registry** (`src/registry.rs`). Constraint groups plus the contract → group reverse index, built once at load so dirty marking is a hash lookup instead of a scan. Relations are `Complement`, `Exhaustive`, `Monotone` (a ladder, weakest claim first), `Implies` (its two-rung case), and `Equivalent` (cross-venue, gated on a `verified` flag a human sets). `Relation::resolution_states` enumerates every resolution a relation permits, and that enumeration — not a hardcoded "a set pays a dollar" — is what every signal is priced against. Loading is in the next section.

**Engine** (`src/engine.rs`). The only place the pieces meet. Apply the event, take the dirty contracts back from the book store as an ordered `Vec`, hand them to the solver, hand what it returns to the sink, publish state. Single owner, no locks: the work per update is microseconds and splitting it across tasks would add channel hops to parallelize nothing. A sequence gap asks the feed to resync through `Feed::request_resync` and evaluates nothing until a snapshot lands — a replay's default is to do nothing, since the recording already contains whatever resync the live run performed. State publishes to a `tokio::broadcast` and is only built when something is subscribed, so a verification replay allocates no snapshots. The sink is a trait; today it logs signals, and phase 6 drops the paper executor in behind it.

## Solver

`Solver::evaluate(registry, books, dirty, fees, config)` in `src/solver/`. Takes the dirty contracts, evaluates their groups in id order, returns opportunities ranked by annualized return. Books and engine time both arrive through the `BookSource` trait, which `BookStore` implements, so the freshness gate reads the same injected clock that stamped the books and decides identically live and under replay.

Four fast paths (`fast.rs`) run on every dirty group and allocate nothing unless they find something. Each is stated in executable prices — what you can actually pay against what you can actually receive — because a quoted inversion that doesn't survive crossing both spreads is not a trade.

| Path | Condition | Trade |
| --- | --- | --- |
| Complement | `ask_yes + ask_no < 100` on one contract | buy both outcomes |
| Exhaustive | sum of member `ask_yes` `< 100` | buy one of each |
| Monotonicity | `ask_yes[i] + ask_no[i+1] < 100`, adjacent rungs | buy the weaker rung's yes, the stronger rung's no |
| Cross-venue | `ask_yes(A) + ask_no(B) < 100`, either direction | buy yes on one venue, no on the other |

Both published forms of the complement check are the same inequality on a Kalshi contract: a yes ask *is* a resting no bid at `100 - P`, so `ask_yes + ask_no = 200 - (bid_yes + bid_no)`. Only the ask form is checked, because that's the form that gets costed.

Costing (`costing.rs`) turns a candidate into a signal: freshness gate, depth walk per leg, size cap at the thinnest leg, per-level fees, net edge, annualized return. Any book that isn't `Live` and inside `max_book_age_ms` skips the whole group. Fees round up once per level consumed rather than once on a blended average — that's what the venue charges, and where it differs it overstates, which can only suppress a marginal signal instead of putting on a losing trade that looked profitable. Payoff is never assumed: `min_payoff_per_unit` enumerates the relation's states and takes the worst, so legs that don't pay in every resolution emit nothing.

Inside the cap, the executable quantity is the one that maximizes net profit, not the cap itself. Deeper levels cost more while the payoff per contract is fixed, so costing only the full cap would throw away arbitrages whose edge lives near the top of book — which is most of them. Price per contract is constant within a level and the per-level fee ceiling only amortizes as size grows, so net profit is monotone inside a level and can only turn over at a boundary; checking boundaries is exact, not a heuristic. When every level is profitable the answer is the cap.

`SolverMetrics` breaks rejections into `not_live`, `stale`, `missing_book`, `unverified`, `no_depth`, `fees_exceed_gap`, `below_min_edge`, `below_min_return`, and `payoff_not_guaranteed`, and every candidate outcome is logged with its group, cost, and reason. Fees are expected to reject the majority, which is only worth claiming if each rejection names one concrete cause. Thresholds come from the `[engine]` block of `config/example.toml` as `SolverConfig` — 500 ms, 1 cent, 15% annualized, 500 contracts — parsed in phase 5.

The Fed example, priced by the real solver:

```
case            leg price     leg cost      leg fee      running
98c (reject)            4          400           27          427
98c (reject)           62         6200          165         6792
98c (reject)           29         2900          145         9837
98c (reject)            3          300           21        10158
 INFO sum100::solver: candidate rejected group=0 reason=fees_exceed_gap legs=4 top_of_book_cost_cents=98 gross_edge_cents=2
98c (reject)   payoff 10000  cost 9800  fees 358  net -158  -> rejected
95c (accept)   payoff 10000  cost 9500  fees 355  net 145  -> accepted, 17.90% annualized
```

Two cents of gross edge against $3.58 of fees. Reproduce with `cargo test --test solver -- --exact fed_example_rejection_table --nocapture`; the captured run is in [docs/phase-4-evidence/](docs/phase-4-evidence/). The general LP fallback for groups no fast path expresses is future work, sketched in `src/solver/mod.rs`. Phase 4 detail, including four places the implementation deliberately departs from its written spec, is in [docs/phase-4-summary.md](docs/phase-4-summary.md).

## Registry

The constraint graph is data, not code. Adding coverage is a pull request against `config/registry.toml`.

```toml
[[event]]
id = "fed-2026-09"
description = "FOMC rate decision, September 2026"
resolves_at = "2026-09-17T18:00:00Z"   # RFC3339; this is what ranking measures against
resolution_source = "FOMC statement"

[[event.group]]
type = "exhaustive"                     # exactly one member pays $1
members = [
  { venue = "kalshi", ticker = "KXFED-26SEP-C50" },
  { venue = "kalshi", ticker = "KXFED-26SEP-C25" },
  { venue = "kalshi", ticker = "KXFED-26SEP-NC" },
]

[[event.group]]
type = "equivalent"                     # cross-venue; verified is required here
verified = false
members = [
  { venue = "kalshi", ticker = "KXFED-26SEP-C25" },
  { venue = "polymarket", token = "0x..." },
]
```

Group types are `complement` (one member), `exhaustive`, `monotone` (a ladder, written weakest claim first), `implies` (antecedent first), and `equivalent`. Kalshi members take `ticker`, Polymarket takes `token`, never both. `verified` is required on `equivalent` and rejected everywhere else, so a cross-venue pair states its status rather than inheriting a default.

**To add an event:** append an `[[event]]` block, list its groups, run `make registry-validate` to check the shape offline, then `make registry-validate-prod` to confirm every ticker is a market Kalshi actually lists. Restart and it is live. No code changes.

Loading touches no network. It parses, interns tickers to contract ids, builds the index, and enforces everything checkable from the file: a ladder must ascend by strike, a contract cannot sit in two exhaustive sets or under two events, an unknown venue or group type is fatal. Confirming those tickers exist is `registry validate --live`, which fetches market metadata and caches it under `cache_dir`. The split is deliberate — replay has to run with no credentials and no network, and it has to resolve the same tickers to the same ids the live run did, so a loader that phoned a venue would break both.

That last point is the sharp edge. `ContractId` is positional, assigned in interning order, and the parser and book store intern from a ticker list too. All three must see the same list in the same order or every id silently shifts, which is why `Registry::tickers(venue)` exists and why `scan` builds everything from it. A test pins it.

`config/example.toml` holds the `[engine]` thresholds, per-venue settings, and the registry path. Unknown keys are rejected: a misspelled `max_positon_size` fails at startup instead of quietly leaving a default in place.

## Execution

This is the only part of the system that can lose money, so the defaults are asymmetric: an unintended paper run costs a rerun, an unintended live run costs cash.

**Four gates** stand between a command line and a real Kalshi order. `--live-orders` on the command, `--live` (a recording can never place anything), `--prod`, and `executor.paper_mode = false` in the config. Miss any one and you get [`PaperOrderClient`](src/exec/paper.rs). The engine checks a fifth time on the path that actually spends: a live client with `allow_live_orders` unset is refused there, not at construction. There is deliberately no Makefile recipe that passes `--live-orders`.

**Fill-or-kill, always.** An arbitrage is one position, not two trades — buying one leg and missing the other turns a risk-free trade into a naked directional bet at a price nobody chose. The usual answer, "cancel the other order", does not work: a *filled* order cannot be cancelled, only unwound by trading back across the spread. Fill-or-kill pushes that to the venue, which can enforce it. When a leg escapes anyway, `ExecutionError::Legged` says so in those words and hands back the fills needing unwind, rather than reporting a tidy cancellation that never happened. Nothing is booked from half a trade, and the counter is called `trades_needing_reconciliation`. A timeout is its own case: the futures are dropped, so whether anything reached the venue is genuinely unknown, and the error says that too.

Legs go out concurrently. That is not an optimization — placing them in sequence would price the second off a book that has already seen the first, which is the move that removes the edge.

**Paper fills walk the book.** Same depth walk and same fee schedule the solver costed with, because filling a 500-contract order at top of book would flatter every result in exactly the direction that makes a bad strategy look good. A book that moved past the limit is a miss, not a fill at a worse price.

**Portfolio** (`src/portfolio.rs`). Capital leaves on entry and comes back as the payoff at settlement. Positions mark at the **bid**, not the mid — valuing at the mid books an unrealized profit the spread takes back on exit, and a leg with no bid at all marks at zero rather than at cost. The daily loss limit resets at calendar UTC midnight, not 24 hours after whatever time the process started, because two restarts in a day must not hand out two fresh budgets. Profits do not offset the day's losses: the limit exists to stop a bad day.

**Health** (`src/health.rs`) is the question the freshness gate cannot answer. A book looks fine while the socket behind it has been dead for a minute — an idle far strike and a dead feed produce identical books. A venue starts disconnected and earns health from data; three consecutive errors stop trading and one success clears them; a specific failure is never overwritten by a vaguer one.

**Risk** (`src/risk.rs`) limits capital *locked*, not expected profit. The solver proves a trade cannot lose at settlement — but only if the contracts resolve the way the registry says, and the registry is a human-maintained file. The real exposure is concentration on one resolution rule being written down wrong, so limits are per event, per theme (all Fed decisions share a source and fail together), and on concurrent positions. Every check asks what exposure *would become*, so one trade cannot step over a limit it was under.

## Recording and replay

Every inbound frame is written **before parsing** to `OUT/{demo,production}/kalshi-YYYY-MM-DD.ndjson.gz`, one file per venue per UTC receipt day, one recorder per directory behind an OS lock. Each line is an envelope — receipt milliseconds, monotonic sequence, kind, raw — and for text the raw payload is a JSON string that decodes back to the original bytes under a single-trailing-LF rule. Whitespace, field order, Unicode escapes, and malformed JSON all survive, because the recorder never parses venue JSON. Feed lifecycle events ride the same stream as `control` envelopes so replay reproduces the live resync path. Files are concatenated gzip members flushed every two seconds: read them with `MultiGzDecoder`, Python `gzip`, or `gzip -dc`. Raw files under `data/` are gitignored; curated fixtures are tracked. Byte-level format, restart, and crash-recovery rules are in [ARCHITECTURE.md](ARCHITECTURE.md) §4.7.

```sh
gzip -t   data/production/kalshi-2026-09-13.ndjson.gz
gzip -dc  data/production/kalshi-2026-09-13.ndjson.gz | head
```

`replay` is a `Feed` implementation, not a mode — same parser, same book store, same code path. `src/clock.rs` holds the only wall-clock read in the crate: live passes `WallClock`, replay passes a `ReplayClock` advanced to each recorded receipt time, never the venue `ts`. `--pace max` runs flat out, `--pace realtime` sleeps the recorded gaps, `--session N` picks a run out of a multi-run day.

Both `dump` and `replay` print a digest: SHA-256 per contract over its canonical book, SHA-256 over the gap log, no timestamps anywhere in it. `replay --verify` compares against a `--digest-out` file and exits nonzero on any mismatch. Four production sessions and 92 book hashes match so far — [docs/phase-3-summary.md](docs/phase-3-summary.md).

## Tests

`make check` is fmt, clippy `-D warnings`, and the suite. Parser and book-store tests replay the full stage 2 capture to a pinned final bid/ask and cover complement conversion, gap and resync transitions, floor drift, and crossed books. Recorder tests assert byte-identical read-back, member concatenation, and sequence continuation across restarts. Signing tests verify RSA-PSS against a throwaway key.

Execution tests (`tests/phase6.rs`) run the whole path: an edge becomes a position becomes a mark, and each gate gets its own test — unhealthy venue, no capital, event limit, closed day reopening at midnight, a legged trade flagged rather than booked, and a live client refused without the opt-in. A real recording replays in paper mode and places nothing.

Engine tests cover registry loading, that the registry, parser, and book store agree on every contract id, dirty marking (including that a gap or disconnect marks nothing, since the solver would only reject it), the loop finding both a crossed book and a ladder inversion in one update, gap-driven resync, and replay determinism — the same capture twice produces byte-identical signal logs and byte-identical serialized state broadcasts.

Solver tests cover all four paths, the Fed example both ways, multi-level depth walking with size capping, the freshness gate, ranking, and the real stage 2 book (coherent, correctly silent). Four property tests run on a seeded splitmix64 so a failure replays exactly:

- coherent groups never signal
- every emitted position pays at least its guaranteed payoff in **every** resolution the relation permits, and strictly more than it cost to enter
- fees never fall with quantity, and splitting an order never beats batching it
- the thinnest leg caps the executable quantity

Fixture provenance and hashes are in `tests/fixtures/`. The one check that isn't automated: run `dump` beside the Kalshi web UI on an open ticker and watch the prices agree — any divergence should be preceded by a logged gap and resync.

---

Kalshi references, checked September 13, 2026: [websockets](https://docs.kalshi.com/getting_started/quick_start_websockets), [API keys](https://docs.kalshi.com/getting_started/api_keys), [orderbook updates](https://docs.kalshi.com/websockets/orderbook-updates), [environments](https://docs.kalshi.com/getting_started/api_environments), [events](https://docs.kalshi.com/api-reference/events/get-events), [markets](https://docs.kalshi.com/api-reference/market/get-markets).
