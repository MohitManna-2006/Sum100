# Sum100

Real-time coherence and arbitrage engine for prediction markets, in Rust. Paper trading only. Phase 2 provides the book store: live yes/no books with `100 - P` complement conversion, subscription-scoped sequence gap detection, forced-reconnect resync, and a `dump` command for visual verification. Phase 3 adds deterministic offline replay: `replay` drives a recorded file through the same parser and book store under an injected clock and verifies final book and gap-log hashes against the live run. Solver integration remains a later phase.

## Design documents and scope

[ARCHITECTURE.md](ARCHITECTURE.md) and [PLAN.md](PLAN.md) contain the imported design
and phased roadmap, reconciled with the approved stage 2 and phase 2 decisions. The original
supplied README is preserved verbatim in [README.reference.md](README.reference.md).
It uses the earlier name Parity and describes planned commands; this README is
the source for current setup and supported behavior. The broader roadmap does
not mean the registry, solver integration, or frontend already exists.

## Setup

Install the Rust toolchain selected by `rust-toolchain.toml`, then run `cargo build --release`.

Put credentials in **`~/.zshenv`**, outside the repository:

```sh
export KALSHI_KEY_ID='your-key-id'
export KALSHI_PRIVATE_KEY_PATH="$HOME/.config/kalshi/key.pem"
```

Non-interactive zsh tooling reads `~/.zshenv` but does not read `~/.zshrc`. If these exports already live in `~/.zshrc`, move them rather than keeping two copies. Open a new shell or run `source ~/.zshenv` in an existing one. Keep the private key outside the repository and restrict its permissions (`chmod 600 ~/.config/kalshi/key.pem`). PKCS#1 and PKCS#8 RSA PEM keys are supported; missing, malformed, or unusable keys fail at startup.

Demo is the default for every command. Production requires **`--prod`**, which logs a warning. Credentials are environment-specific: a production key cannot authenticate against demo. Every WebSocket connection requires authentication, including public channels. There are no order-placement, execution, or transfer endpoints in this program. REST discovery uses only public `GET /events` and `GET /markets` requests.

```sh
# Use a currently open demo ticker with demo credentials.
cargo run --release -- record --venue kalshi --tickers YOUR-DEMO-TICKER

# Live best bid/ask table (also records raw bytes under --out).
cargo run --release -- dump --venue kalshi --prod \
  --tickers KXBTCD-26SEP1417-T76999.99 --out data --seconds 70

# Read-only production recording with a production key; graceful timed shutdown.
cargo run --release -- record --venue kalshi --prod \
  --tickers KXBTCD-26SEP1417-T76999.99 --out data --seconds 70

# Live dump that also writes its final state digest for replay verification.
cargo run --release -- dump --venue kalshi --prod \
  --tickers KXBTCD-26SEP1417-T76999.99 --out data --seconds 180 --digest-out live.digest

# Offline replay (no credentials, no network) verified against that live run.
cargo run --release -- replay --venue kalshi \
  --file data/production/kalshi-2026-09-14.ndjson.gz --verify --expect-file live.digest

# Multiple tickers are comma-separated. No --seconds means run until Ctrl-C.
cargo run --release -- probe --prod --ticker KXBTCD-26SEP1417-T76999.99
cargo run --release -- markets --prod --series KXBTCD
```

The dated ticker above is a capture example, not a permanent default. Discover a current market before later runs. `markets` follows event and market cursor pagination and prints one market per line. HTTP 429 and server errors receive up to five retries with backoff; `Retry-After` is honored up to a 30-second wait, with longer delays returned as an explicit error. `probe` prints ticker, trade, and orderbook payloads. The original `--bin probe` remains as the stage 1 diagnostic; use the `sum100 probe` subcommand for the supported CLI.

Use `RUST_LOG=debug` to see normalized events and skipped types. The default is `info`, with metrics every 30 seconds. `config/example.toml` remains a future configuration sketch; current options come from the CLI and credential environment variables. `--tickers` is required for recording so a missing list cannot silently select an expired market.

## Feed boundary

`Feed::next` returns `FeedEvent` asynchronously and can be used through `dyn Feed`. A boxed future supplies that interface without adding `async-trait`. `ContractId` is an interned `u32`, with a separate venue/ticker lookup map.

Snapshots carry **`yes` and `no` resting levels**, preserving venue prices. Deltas carry `Side::Yes` or `Side::No` and a signed `size_delta`. The feed does not convert no prices; the book store owns the `100 - P` identity. Sequence numbers are venue subscription sequence numbers; they can span multiple tickers and reset after a new subscription. Consumers must invalidate state on `Disconnected` and wait for a fresh snapshot after `Resubscribed`.

Prices go directly from decimal strings to integer cents; sub-cent precision and values outside 0–100 cents are errors. Size strings floor to whole contracts using integer arithmetic. Thus `491.90` becomes `491`, and signed `-1.20` becomes `-2`. Floor understates depth, but repeated fractional deltas can accumulate conservative drift: `floor(snapshot) + sum(floor(delta))` is not generally the same as flooring the resulting exact book. The book store clamps a level at zero when floor drift would go negative, counts the clamp, and stays `Live` (understating depth is the safe direction). `discarded_size_hundredths` measures the sum of `x - floor(x)` in hundredths across successfully parsed messages, not unique missing depth. Nonzero precision beyond hundredths is rejected. Snapshot timestamps fall back to local receipt time when absent; deltas use venue milliseconds or their RFC3339 timestamp.

Unknown types increment `unknown_messages` and `parse_attempts`, log at debug, and produce no event. Malformed known payloads increment `parse_errors`, log the raw payload at warn, and do not stop recording. Other metrics cover received messages, successful reconnects, uncompressed recorded bytes, discarded size, clock skew, and integer latency buckets. Reconnection uses exponential backoff from 250 ms to 30 s with positive jitter, resubscribes the complete ticker list, and emits `Resubscribed` only after the venue acknowledges the subscription. Feed consumers should continuously drain events; the channel is bounded.

## Book store

`BookStore` owns in-memory books. Each book keeps dense `[i64; 101]` size arrays for resting yes bids and no bids. Yes asks are derived as `price = 100 - no_price`. Snapshots are absolute and always applied; they set the book `Live` and rebase the subscription expected sequence. Deltas apply only when the book is `Live` and `seq` equals the expected subscription sequence. A gap marks every live book `Resyncing`, leaves level contents unchanged, and returns `Applied::Gap`. The driver (for example `dump`) calls `KalshiFeed::request_resync()`, which drops the socket so the existing reconnect path delivers a fresh snapshot. Sequence tracking is subscription-scoped for the single orderbook subscription the feed opens today; multiple concurrent `sid`s are not modeled yet.

A crossed book (`best yes bid + best no bid > 100`) stays `Live` and increments `crossed_books_observed` — that condition is the complement arbitrage the later solver fast path will consume. Book metrics also cover snapshots applied, deltas applied, deltas skipped while not live, sequence gaps, resync requests, and negative-level clamps. `dump` prints a monospace best-bid/ask table to stdout on an interval (tracing stays on stderr) and records raw envelopes like `record`.

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

Live visual check: run `dump` beside the Kalshi web interface for an open ticker and confirm best prices match; any divergence should be preceded by a logged sequence gap and resync.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Implementation references: [WebSocket authentication and subscription](https://docs.kalshi.com/getting_started/quick_start_websockets), [RSA-PSS signing](https://docs.kalshi.com/getting_started/api_keys), [orderbook updates](https://docs.kalshi.com/websockets/orderbook-updates), [API environments](https://docs.kalshi.com/getting_started/api_environments), [event discovery](https://docs.kalshi.com/api-reference/events/get-events), and [market discovery](https://docs.kalshi.com/api-reference/market/get-markets), checked September 13, 2026. Reqwest's `query` feature enables proper cursor/ticker URL encoding; no new top-level dependencies were added in phase 2.
