# Sum100

Real-time coherence and arbitrage engine for prediction markets, in Rust. Paper trading only. Phase 2 provides the book store: live yes/no books with `100 - P` complement conversion, subscription-scoped sequence gap detection, forced-reconnect resync, and a `dump` command for visual verification. Phase 3 adds deterministic offline replay: `replay` drives a recorded file through the same parser and book store under an injected clock and verifies final book and gap-log hashes against the live run. Phase 4 adds the solver: four closed-form constraint checks, a depth-aware and fee-aware costing pipeline, and ranking by annualized return on locked capital. Registry loading, the paper executor, and a second venue remain later phases.

## Design documents and scope

[ARCHITECTURE.md](ARCHITECTURE.md) and [PLAN.md](PLAN.md) contain the imported design
and phased roadmap, reconciled with the approved stage 2 and phase 2 decisions. The
estimation path is specified in [docs/COHERENCE.md](docs/COHERENCE.md); it is not
implemented yet. The original
supplied README is preserved verbatim in [README.reference.md](README.reference.md).
It uses the earlier name Parity and describes planned commands; this README is
the source for current setup and supported behavior. The broader roadmap does
not mean registry loading, the executor, or the frontend already exists.

## Setup

Install the Rust toolchain selected by `rust-toolchain.toml`, then run `cargo build --release`.

Put credentials in **`~/.zshenv`**, outside the repository:

```sh
export KALSHI_KEY_ID='your-key-id'
export KALSHI_PRIVATE_KEY_PATH="$HOME/.config/kalshi/key.pem"
```

Non-interactive zsh tooling reads `~/.zshenv` but does not read `~/.zshrc`. If these exports already live in `~/.zshrc`, move them rather than keeping two copies. Open a new shell or run `source ~/.zshenv` in an existing one. Keep the private key outside the repository and restrict its permissions (`chmod 600 ~/.config/kalshi/key.pem`). PKCS#1 and PKCS#8 RSA PEM keys are supported; missing, malformed, or unusable keys fail at startup.

Demo is the default for every command. Production requires **`--prod`**, which logs a warning. Credentials are environment-specific: a production key cannot authenticate against demo. Every WebSocket connection requires authentication, including public channels. There are no order-placement, execution, or transfer endpoints in this program. REST discovery uses only public `GET /events` and `GET /markets` requests.

The clap CLI in `src/main.rs` is the source of flags. A root `Makefile` wraps `cargo run --release --` for the recipes below. It does not pin a ticker and selects production only on `*-prod` targets. `make help` lists knobs (`TICKERS`, `SECONDS`, `FILE`, `DIGEST`, `EXTRA`, and others). `--out data` is already the CLI default.

```sh
# Use a currently open demo ticker with demo credentials.
make record TICKERS=YOUR-DEMO-TICKER

# Live best bid/ask table (also records raw bytes under --out).
make dump-prod TICKERS=KXBTCD-26SEP1417-T76999.99 SECONDS=70

# Read-only production recording with a production key; graceful timed shutdown.
make record-prod TICKERS=KXBTCD-26SEP1417-T76999.99 SECONDS=70

# Live dump that also writes its final state digest for replay verification.
make dump-digest TICKERS=KXBTCD-26SEP1417-T76999.99 SECONDS=180

# Offline replay (no credentials, no network) verified against that live run.
make replay FILE=data/production/kalshi-2026-09-14.ndjson.gz

# Multiple tickers are comma-separated. No SECONDS means run until Ctrl-C.
make probe-prod TICKER=KXBTCD-26SEP1417-T76999.99
make markets-prod SERIES=KXBTCD
```

Those recipes are equivalent to `cargo run --release -- dump --venue kalshi --prod --tickers ...`. Pass through extra CLI flags with `EXTRA='--interval-ms 250'`. `FILE` defaults to today's UTC daily production recording if omitted.

The dated ticker above is a capture example, not a permanent default. Discover a current market before later runs. `markets` follows event and market cursor pagination and prints one market per line. HTTP 429 and server errors receive up to five retries with backoff; `Retry-After` is honored up to a 30-second wait, with longer delays returned as an explicit error. `probe` prints ticker, trade, and orderbook payloads. The original `--bin probe` remains as the stage 1 diagnostic; use the `sum100 probe` subcommand for the supported CLI.

Use `RUST_LOG=debug` to see normalized events and skipped types. The default is `info`, with metrics every 30 seconds. `config/example.toml` remains a future configuration sketch; current options come from the CLI and credential environment variables. `--tickers` is required for recording so a missing list cannot silently select an expired market.

## Feed boundary

`Feed::next` returns `FeedEvent` asynchronously and can be used through `dyn Feed`. A boxed future supplies that interface without adding `async-trait`. `ContractId` is an interned `u32`, with a separate venue/ticker lookup map.

Snapshots carry **`yes` and `no` resting levels**, preserving venue prices. Kalshi omits the key of a side with no resting orders (common for far strikes); that side is applied as empty, counted in `snapshot_sides_absent`, and the snapshot's sequence number is accepted. An explicit `null` side is also empty. A snapshot with both side keys absent, a side that is not null or a list of `[price, size]` string pairs, or a bad row is still a parse error. Deltas carry `Side::Yes` or `Side::No` and a signed `size_delta`. The feed does not convert no prices; the book store owns the `100 - P` identity. Sequence numbers are venue subscription sequence numbers; they can span multiple tickers and reset after a new subscription. Consumers must invalidate state on `Disconnected` and wait for a fresh snapshot after `Resubscribed`.

Prices go directly from decimal strings to integer cents; sub-cent precision and values outside 0–100 cents are errors. Size strings floor to whole contracts using integer arithmetic. Thus `491.90` becomes `491`, and signed `-1.20` becomes `-2`. Floor understates depth, but repeated fractional deltas can accumulate conservative drift: `floor(snapshot) + sum(floor(delta))` is not generally the same as flooring the resulting exact book. The book store clamps a level at zero when floor drift would go negative, counts the clamp, and stays `Live` (understating depth is the safe direction). `discarded_size_hundredths` measures the sum of `x - floor(x)` in hundredths across successfully parsed messages, not unique missing depth. Nonzero precision beyond hundredths is rejected. Events carry `venue_ts_ms`, the venue's own timestamp and never local receipt time: deltas use venue milliseconds or their RFC3339 `ts`; Kalshi snapshots carry no timestamp, so theirs is `None`. Receipt time comes from the injected clock and the envelope's `received_at_ms`; the raw recorded payload preserves the venue timestamp as well.

Unknown types increment `unknown_messages` and `parse_attempts`, log at debug, and produce no event. Malformed known payloads increment `parse_errors`, log the raw payload at warn, and do not stop recording. Other metrics cover received messages, successful reconnects, uncompressed recorded bytes, discarded size, clock skew, and integer latency buckets. Reconnection uses exponential backoff from 250 ms to 30 s with positive jitter, resubscribes the complete ticker list, and emits `Resubscribed` only after the venue acknowledges the subscription. Feed consumers should continuously drain events; the channel is bounded.

## Book store

`BookStore` owns in-memory books. Each book keeps dense `[i64; 101]` size arrays for resting yes bids and no bids. Yes asks are derived as `price = 100 - no_price`. Snapshots are absolute and always applied; they set the book `Live` and rebase the subscription expected sequence. Deltas apply only when the book is `Live` and `seq` equals the expected subscription sequence. A gap marks every live book `Resyncing`, leaves level contents unchanged, and returns `Applied::Gap`. The driver (for example `dump`) calls `KalshiFeed::request_resync()`, which drops the socket so the existing reconnect path delivers a fresh snapshot. Sequence tracking is subscription-scoped for the single orderbook subscription the feed opens today; multiple concurrent `sid`s are not modeled yet.

A crossed book (`best yes bid + best no bid > 100`) stays `Live` and increments `crossed_books_observed` — that condition is the complement arbitrage the solver's fast path consumes. Book metrics also cover snapshots applied, deltas applied, deltas skipped while not live, sequence gaps, resync requests, and negative-level clamps. `dump` prints a monospace best-bid/ask table to stdout on an interval (tracing stays on stderr) and records raw envelopes like `record`.

## Registry

`Registry` (`src/registry.rs`) holds the constraint groups and the reverse index from contract to group ids that makes dirty marking a hash lookup rather than a scan. A `Relation` is `Complement`, `Exhaustive`, `Monotone` (a threshold ladder, ordered weakest claim first), `Implies` (the two-rung case of the same thing), or `Equivalent` (a cross-venue pair, gated on a `verified` flag a human sets). `Relation::resolution_states` enumerates every resolution a relation permits, and that enumeration — not a hardcoded "a set pays one dollar" — is what every signal is priced against. Group members are derived from the relation at construction so the two cannot drift apart, and malformed groups (too few members, a contract listed twice) are rejected there rather than at query time. Loading `config/registry.toml` and reconciling it against venue metadata is phase 5; nothing in this module reads a file.

## Solver

`Solver::evaluate(registry, books, dirty, fees, config)` (`src/solver/`) takes the dirty contracts, collects the groups containing them in group-id order, and returns opportunities ranked by annualized return. It reads books through the `BookSource` trait, implemented by `BookStore`, and takes engine time from the same injected clock that stamps `updated_at_ms`, so the freshness gate decides identically live and under replay. Data flows one way: books in, opportunities out, no calls backwards.

**Four fast paths** (`fast.rs`) run on every dirty group and allocate nothing unless they find something. Each is stated in executable prices — what you can actually pay against what you can actually receive — because a quoted inversion that does not survive crossing both spreads is not a trade.

| Path | Condition | Trade |
| --- | --- | --- |
| Complement | `ask_yes + ask_no < 100` on one contract | buy both outcomes |
| Exhaustive | sum of member `ask_yes` `< 100` | buy one of each |
| Monotonicity | `ask_yes[i] + ask_no[i+1] < 100` for an adjacent rung pair | buy the weaker rung's yes, buy the stronger rung's no |
| Cross-venue | `ask_yes(A) + ask_no(B) < 100`, either direction | buy yes on one venue, no on the other |

For a single Kalshi contract the two published forms of the complement check are the same inequality: a yes ask is a resting no bid at `100 - P`, so `ask_yes + ask_no = 200 - (bid_yes + bid_no)`. Only the ask form is checked, because that is the form that gets costed. Every violating adjacent pair in a ladder becomes its own candidate, and a cross-venue pair crossed in both directions yields two independent trades that consume different sides of both books.

**Costing** (`costing.rs`) turns a candidate into a signal, in this order: freshness gate (every member book `Live` and inside `max_book_age_ms`, or the group is skipped whole), depth walk per leg, size cap at the thinnest leg, per-level fee application, net edge, annualized return. Fees round up once per level consumed rather than once on a blended average, because that is what the venue charges and, where it differs, it overstates — overstating can only suppress a marginal signal, while understating puts on a losing trade that looked profitable. Payoff is never assumed: `min_payoff_per_unit` enumerates the relation's resolution states and takes the worst, so a fast path that produced legs which do not pay in every state emits nothing and increments `rejected_payoff_not_guaranteed`.

Within the thinnest-leg cap, the executable quantity is the one that maximizes net profit rather than always the cap itself. Deeper levels cost more while the guaranteed payoff per contract is fixed, so costing the full cap and rejecting on a negative total would discard genuine arbitrages whose edge exists only near the top of book — which is most of them. Because price per contract is constant inside a level and the per-level fee ceiling only amortizes as quantity grows, net profit rises monotonically within a level and can only turn over where some leg steps to a worse price; evaluating the level boundaries is therefore exact rather than a heuristic. When every level is profitable the answer is the cap, which is the published behavior.

**Rejection breakdown.** `SolverMetrics` counts `not_live`, `stale`, `missing_book`, `unverified`, `no_depth`, `fees_exceed_gap`, `below_min_edge`, `below_min_return`, and `payoff_not_guaranteed`, and every candidate outcome is logged at info with its group, leg count, top-of-book cost, and reason. The expected result on live data is that fees reject the majority, which is only a defensible claim if each rejection is attributed to one concrete cause.

**Configuration** is the `[engine]` block of `config/example.toml`, in memory as `SolverConfig`: `max_book_age_ms` 500, `min_net_edge_cents` 1, `min_annualized_return` 0.15, `max_position_size` 500. Parsing the file into it is phase 5.

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

Reproduce with `cargo test --test solver -- --exact fed_example_rejection_table --nocapture`; the captured run is in [docs/phase-4-evidence/](docs/phase-4-evidence/). Not implemented: the general linear-programming fallback for groups no fast path expresses, documented as future work in `src/solver/mod.rs`. Phase 4 details and the places where the implementation departs from its written spec are in [docs/phase-4-summary.md](docs/phase-4-summary.md).

## Recording format

Files are `OUT/demo/kalshi-YYYY-MM-DD.ndjson.gz` or `OUT/production/kalshi-YYYY-MM-DD.ndjson.gz`, using the local receipt's UTC day. An OS lock permits one recorder per environment/output directory. Raw files under `data/` are gitignored; curated fixtures are tracked separately.

Each decompressed line is an envelope with `received_at_ms`, monotonically increasing `sequence`, `kind`, and `raw`. For `kind: "text"`, `raw` is a JSON **string**, escaped only to fit the envelope. The recorder never parses the venue JSON. Decoding that string restores every input byte except **one trailing LF**, if present. Whitespace, field order, numeric spelling, Unicode escapes, malformed JSON, and embedded newlines all survive. There are no blank separator lines. Binary, ping, pong, and close payload bytes are stored as base64 with corresponding `*_base64` kinds; a close payload includes its two-byte status code and reason.

Feed lifecycle events that have no venue bytes are recorded in the same stream as `kind: "control"` envelopes whose `raw` string is a JSON object tagged by `event`: `session_started` (environment and tickers, once per process), `disconnected` (with a reason, immediately before the feed emits `Disconnected`), `reconnected` (attempt number), and `resubscribed` (tickers, immediately before the feed emits `Resubscribed`). The envelope `kind` is the tag, so venue text can never be mistaken for a control event. Files recorded before phase 3 contain no control envelopes and still parse.

Every two seconds, and on graceful shutdown, the recorder finishes and syncs a gzip member. Files contain concatenated members: use `flate2::read::MultiGzDecoder`, Python `gzip`, or `gzip -dc`. `read_records` supplies a streaming envelope reader. Restarting validates the existing day's file and resumes above its last sequence; a new process on a new day may start at 1, so replay identity includes the daily filename. Receipt timestamps do not define sequence order.

A crash can leave an incomplete final member; completed members remain recoverable. An unreadable existing tail causes an explicit error on append rather than silently hiding the corruption. Retain the original file and recover its valid prefix before resuming into that directory. Concurrent writers are rejected. Disk errors stop the feed rather than allow unrecorded parsing.

```sh
# Check gzip integrity, then inspect envelopes.
gzip -t data/production/kalshi-2026-09-13.ndjson.gz
gzip -dc data/production/kalshi-2026-09-13.ndjson.gz | head
```

## Replay

`replay` is a `Feed` implementation, not a mode. It streams the gzip file, passes each `text` payload to the same `Parser::parse` call the live feed makes with the recorded receipt time, and turns control envelopes back into `Disconnected` and `Resubscribed`. The `Clock` trait (`src/clock.rs`) is the only wall-clock read in the crate: live commands pass `WallClock`; replay passes a `ReplayClock` advanced to each event's local receipt time (never the venue `ts`). Book `updated_at_ms` comes from that clock.

`--pace max` (default) runs flat out; `--pace realtime` sleeps recorded inter-arrival gaps. A daily file can hold several runs: each marked run is a session, and records before the first marker form one unmarked session. Pass `--session N` when a file holds more than one; unmarked sessions also need `--tickers`. A run that crosses UTC midnight is split across two daily files and cannot be replayed as one session.

`dump` and `replay` apply events through the same `GapLog::apply` step and print a digest: a SHA-256 per contract over its canonical book (state, seq, and nonzero yes/no levels), a SHA-256 over the gap log (gaps, disconnects, resubscribes, and snapshot resyncs, each keyed by event position), and informational book metrics. No timestamps enter the digest. `dump` stops its feed and applies every already-recorded event before printing, so its digest describes exactly the recorded stream. `replay --verify` compares hashes from `--expect-file` (a `--digest-out` file) or `--expect-book TICKER=SHA256` and `--expect-gaps SHA256`, and exits nonzero on any mismatch. Evidence is in [docs/phase-3-summary.md](docs/phase-3-summary.md).

## Fixtures and verification

`tests/fixtures/` contains verbatim original stage 1 captures, including the complete previously truncated snapshot, and a fresh stage 2 live orderbook session. See its README for provenance and hashes. Parser tests read files from disk. Book-store tests replay the full stage 2 session to a pinned final best bid/ask, and cover complement conversion, gap/resync transitions, level lifecycle, floor-drift clamping, and crossed books. Recorder tests assert byte-identical raw read-back (after the single-LF rule), no blank lines, gzip member concatenation, sequence continuation on restart, and daily rotation. Signing tests generate a throwaway key and verify RSA-PSS signatures against its public key; random salt is not pinned.

Solver tests (`tests/solver.rs`) cover all four fast paths on synthetic books, the Fed example in both directions, multi-level depth walking with size capping, the freshness gate, ranking, and the real stage 2 capture (whose final book is coherent and correctly produces nothing). Four property tests use a seeded splitmix64 generator so a failure replays exactly: coherent groups never signal, every emitted position pays at least its guaranteed payoff in every resolution the relation permits and strictly more than it cost to enter, fees never fall with quantity and splitting an order never beats batching it, and the thinnest leg caps the executable quantity.

Live visual check: run `dump` beside the Kalshi web interface for an open ticker and confirm best prices match; any divergence should be preceded by a logged sequence gap and resync.

```sh
make check
make build
```

Implementation references: [WebSocket authentication and subscription](https://docs.kalshi.com/getting_started/quick_start_websockets), [RSA-PSS signing](https://docs.kalshi.com/getting_started/api_keys), [orderbook updates](https://docs.kalshi.com/websockets/orderbook-updates), [API environments](https://docs.kalshi.com/getting_started/api_environments), [event discovery](https://docs.kalshi.com/api-reference/events/get-events), and [market discovery](https://docs.kalshi.com/api-reference/market/get-markets), checked September 13, 2026. Reqwest's `query` feature enables proper cursor/ticker URL encoding; no new top-level dependencies were added in phase 2.
