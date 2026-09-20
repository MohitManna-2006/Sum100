# Sum100

Prediction markets routinely disagree with themselves. Four mutually exclusive outcomes trade for 96 cents on the dollar. A threshold ladder inverts. The same event prices differently on two venues. Sum100 reads those books live, in Rust, and prices every gap it finds until almost all of them die.

Fees kill most of them. That's the interesting part.

Paper only. There is no order-placement code path and there won't be one.

**Runs today:** authenticated Kalshi feed and raw recorder, book store with subscription-scoped gap detection and resync, byte-exact offline replay, and the solver — four closed-form constraint checks, depth- and fee-aware costing, ranked by annualized return on locked capital.
**Doesn't yet:** registry loading, paper executor, Polymarket, the coherence engine ([docs/COHERENCE.md](docs/COHERENCE.md)), UI.

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

**Registry** (`src/registry.rs`). Constraint groups plus the contract → group reverse index, built once at load so dirty marking is a hash lookup instead of a scan. Relations are `Complement`, `Exhaustive`, `Monotone` (a ladder, weakest claim first), `Implies` (its two-rung case), and `Equivalent` (cross-venue, gated on a `verified` flag a human sets). `Relation::resolution_states` enumerates every resolution a relation permits, and that enumeration — not a hardcoded "a set pays a dollar" — is what every signal is priced against. Reading `config/registry.toml` is phase 5.

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

Solver tests cover all four paths, the Fed example both ways, multi-level depth walking with size capping, the freshness gate, ranking, and the real stage 2 book (coherent, correctly silent). Four property tests run on a seeded splitmix64 so a failure replays exactly:

- coherent groups never signal
- every emitted position pays at least its guaranteed payoff in **every** resolution the relation permits, and strictly more than it cost to enter
- fees never fall with quantity, and splitting an order never beats batching it
- the thinnest leg caps the executable quantity

Fixture provenance and hashes are in `tests/fixtures/`. The one check that isn't automated: run `dump` beside the Kalshi web UI on an open ticker and watch the prices agree — any divergence should be preceded by a logged gap and resync.

---

Kalshi references, checked September 13, 2026: [websockets](https://docs.kalshi.com/getting_started/quick_start_websockets), [API keys](https://docs.kalshi.com/getting_started/api_keys), [orderbook updates](https://docs.kalshi.com/websockets/orderbook-updates), [environments](https://docs.kalshi.com/getting_started/api_environments), [events](https://docs.kalshi.com/api-reference/events/get-events), [markets](https://docs.kalshi.com/api-reference/market/get-markets).
