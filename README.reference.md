# Parity

A real-time coherence and arbitrage engine for prediction markets, written in Rust.

Parity ingests live order books from Kalshi and Polymarket, models the logical relationships between contracts, and continuously checks whether their prices are mutually consistent. When they are not, it computes the exact portfolio that captures the inconsistency, prices it after fees and available depth, and ranks it by return per day of capital locked up.

Paper trading only. No real orders are ever placed.

---

## The idea in sixty seconds

A prediction market contract pays exactly one dollar if an event happens and zero if it does not. Its price is therefore an implied probability. A contract at 45 cents means the market believes there is a 45 percent chance.

Because prices are probabilities, sets of related contracts are not free to take any values they like. They must obey arithmetic:

- A contract and its negation must sum to one dollar.
- A set of mutually exclusive and exhaustive outcomes must sum to one dollar.
- If outcome A implies outcome B, then A must never trade above B.
- The same event listed on two venues must trade at the same price.

When any of these break, the inconsistency itself is a risk-free position. Buy the whole set for less than a dollar, collect a dollar when it resolves, and you did not need to predict anything.

Parity is the machine that watches for that, all day, across thousands of markets.

---

## Why this is harder than comparing prices

Almost every apparent arbitrage in prediction markets is fake. Parity exists because of the three reasons why.

### Fees are larger than most gaps

Kalshi's taker fee follows the shape `0.07 x C x P x (1 - P)`, rounded up to the next cent, where C is contract count and P is price in dollars. The `P x (1 - P)` term peaks at 50 cents, which means the fee is largest exactly where most liquid markets trade. Polymarket uses a similarly probability-dependent curve.

A worked example that looks like free money and is not:

| Outcome | Best ask |
|---|---|
| Cut 50bps or more | 4c |
| Cut 25bps | 62c |
| No change | 29c |
| Hike | 3c |
| **Total** | **98c** |

Buying 100 of each costs $98.00 and guarantees $100.00 at resolution. Two dollars of apparent edge.

Now the fees on those four legs:

| Leg | Fee on 100 contracts |
|---|---|
| 4c | $0.27 |
| 62c | $1.65 |
| 29c | $1.45 |
| 3c | $0.21 |
| **Total** | **$3.58** |

Net result is a loss of $1.58. The 62 cent leg alone costs more than the entire edge. This trade needed the asks to sum to 96.4 cents or lower, not 98.

A price-only scanner emits this signal. Parity rejects it. That difference is most of the project.

### Capital is locked until resolution

Profit is not realized when the trade is placed. It arrives when the event settles. A three cent edge on a market resolving in eight months is a worse use of capital than a one cent edge resolving tomorrow.

Parity ranks every opportunity by annualized return on locked capital, never by raw edge.

### Most violations are stale quotes

During fast price moves, one venue's feed runs momentarily behind the other. The gap you see is not a gap, it is latency. Parity gates every signal on per-venue freshness and sequence continuity, and tracks how many candidate signals were rejected for staleness as a first-class metric.

---

## What it detects

| Type | Constraint | Scope |
|---|---|---|
| Complement | `yes + no = 100c` | Single contract |
| Exhaustive set | `sum of all outcomes = 100c` | Single venue, one event |
| Monotonicity | `P(A) <= P(B)` when A implies B | Ladder markets |
| Cross venue | Same event, same price | Both venues |

The first three live entirely inside one venue, which means the system produces real findings before any cross-venue matching exists. The fourth is the hardest and comes last.

---

## Quick start

```bash
git clone https://github.com/YOUR_USERNAME/parity
cd parity
cp config/example.toml config/local.toml
cargo run --release -- record --venue kalshi
```

That records live market data to disk. Kalshi's public channels need no authentication, so this works immediately.

To scan the recorded data offline:

```bash
cargo run --release -- scan --replay data/kalshi-2026-09-13.jsonl.gz
```

To scan live:

```bash
cargo run --release -- scan --live
```

To run the web interface alongside the engine:

```bash
cargo run --release -- scan --live --serve 0.0.0.0:8080
```

Then open `http://localhost:8080`.

### Credentials

Only Kalshi's private order book channel requires authentication. Generate an API key in your Kalshi account settings, download the private key file, and set:

```bash
export KALSHI_KEY_ID=your-key-id
export KALSHI_PRIVATE_KEY_PATH=/path/to/key.pem
```

Polymarket's public market data requires no credentials. Wallet signing is only needed for placing orders, which Parity never does.

---

## Configuration

`config/local.toml` controls everything the engine does not discover at runtime.

```toml
[engine]
max_book_age_ms = 500
min_net_edge_cents = 1
min_annualized_return = 0.15
max_position_size = 500

[venues.kalshi]
enabled = true
ws_url = "wss://api.elections.kalshi.com/trade-api/ws/v2"
fee_multiplier = 1.0

[venues.polymarket]
enabled = false
clob_url = "https://clob.polymarket.com"

[recorder]
enabled = true
output_dir = "data"
compress = true
```

Contract groupings live separately in `config/registry.toml` so that adding a new event does not require a recompile. See ARCHITECTURE.md for the registry schema.

---

## Architecture at a glance

```
  venue feeds  ->  book store  ->  solver  ->  paper executor
       |                              ^
       v                              |
    recorder                      registry
```

- **Venue feeds** own the network connections and translate venue-specific messages into one internal format.
- **Book store** applies order book deltas in sequence order and detects gaps.
- **Registry** holds which contracts are related and how, plus their resolution rules.
- **Solver** re-checks only the constraint groups touched by each price change, applies fees and depth, and emits opportunities.
- **Paper executor** simulates fills and tracks profit and loss.
- **Recorder** writes every raw message to disk so any session can be replayed.

The key structural decision is that live mode and replay mode are the same code path. A recorded file and a live websocket both implement the same feed trait, so the backtest exercises production logic exactly.

Full detail in [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Project layout

```
src/
  main.rs            CLI entry, wiring
  types.rs           Cents, Venue, Level, Book
  feed/
    mod.rs           the trait live and replay both implement
    kalshi.rs        websocket client, auth, message parsing
    polymarket.rs    CLOB client and message parsing
    replay.rs        reads recorded files as a feed
  book.rs            delta application, sequence gap detection
  registry.rs        contract groups, relations, resolution metadata
  fees/
    mod.rs           FeeModel trait
    kalshi.rs
    polymarket.rs
  solver/
    mod.rs           dirty marking, dispatch
    fast.rs          complement, exhaustive, monotonicity, cross venue
    lp.rs            general linear program fallback
  exec.rs            paper executor, position and PnL tracking
  record.rs          writes raw messages to disk
  api.rs             websocket and REST endpoints for the frontend
  metrics.rs         latency histograms, counters
web/                 Vite, React, TypeScript frontend
config/
  example.toml
  registry.toml
data/                recorded sessions, gitignored
```

---

## Performance

All figures are measured on the recorded replay corpus, not estimated. Numbers below are placeholders until the benchmark suite is run.

| Metric | Value |
|---|---|
| Messages processed per second | TBD |
| Tick to signal p50 | TBD |
| Tick to signal p99 | TBD |
| Constraint groups evaluated per second | TBD |
| Contracts tracked concurrently | TBD |
| Signals rejected for staleness | TBD |
| Memory footprint steady state | TBD |

Reproduce with:

```bash
cargo bench
cargo run --release -- scan --replay data/bench-corpus.jsonl.gz --report
```

---

## Testing

```bash
cargo test          # unit and integration
cargo clippy        # lints
cargo fmt --check   # formatting
```

Three categories of test carry most of the weight.

**Worked example tests.** The Fed decision example above is encoded as a test asserting the solver returns nothing, because fees exceed the gap. A second version with a five cent gap asserts it returns an opportunity. A third asserts that a stale leg is always rejected.

**Property tests.** The central invariant is that any opportunity the solver emits must have non-negative payoff in every possible resolution of the event, and strictly negative cost. Property testing generates random price configurations and asserts this holds. It finds edge cases hand-written tests miss.

**Replay regression tests.** A small recorded corpus is checked into the repository. Running the solver over it must produce a byte-identical signal log. Any change to the solver that alters historical output shows up immediately in a diff.

---

## Disclaimer

Parity is a research and engineering project. It places no real orders, holds no funds, and is not financial advice. Prediction market arbitrage is competitive, edges are small and often not executable at size, and fees frequently exceed apparent gaps. Anyone considering trading these markets with real money should not treat this repository as a strategy.

---

## Roadmap

- [x] Core types and feed abstraction
- [ ] Kalshi feed with authentication and recording
- [ ] Book store with sequence gap detection
- [ ] Replay feed
- [ ] Fast path solver for single-venue constraints
- [ ] Fee models for both venues
- [ ] Registry with resolution rule diffing
- [ ] Paper executor and PnL tracking
- [ ] Polymarket feed
- [ ] Cross-venue contract matching
- [ ] Web interface
- [ ] General LP fallback for irregular constraint clusters

See [PLAN.md](PLAN.md) for the phased build schedule.

---

## License

MIT
