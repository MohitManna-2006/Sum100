# Sum100 Production Risk Register & Remediation Plan

This document records the failure modes that must be addressed before Sum100 is
trusted with production capital, especially before adding Polymarket and
cross-venue execution.

The central principle is simple:

> A price discrepancy is not an arbitrage opportunity unless the data is fresh,
> the contracts are semantically identical, the entire intended quantity is
> executable after fees, and the resulting position is either atomically hedged
> or recoverable at a bounded loss.

This is a design and engineering backlog, not a claim that every safeguard is
already implemented. The current repository is Kalshi-first. Polymarket
discovery, cross-venue matching, cross-venue balances, and cross-venue order
execution are future work.

## Current implementation context

The current engine already contains useful Kalshi-side safeguards:

- WebSocket sequence gaps move books into a resynchronization state instead of
  silently applying potentially corrupt deltas.
- Book freshness is checked before solver evaluation.
- The solver walks available depth rather than assuming that the best ask is
  available for the entire requested quantity.
- Fees are included in candidate pricing.
- Paper execution supports fill-or-kill behavior and records the actual fills.
- Live order placement requires explicit CLI/configuration opt-ins.
- Auto-discovered groups are marked as inferred and are blocked from live order
  execution unless explicitly allowed.
- Discovery caches are timestamped and scoped so a series-only cache cannot be
  mistaken for a full-universe cache.

Those controls reduce risk, but they do not solve the cross-venue problems in
this document. In particular, a local timeout, a stale-data check, or a
fill-or-kill request does not create a transaction spanning two independent
exchanges.

---

## Category 1: Latency and data-pipeline disconnects

### Scenario A: Stale snapshot execution

#### Vulnerability

The incoming WebSocket stream can be delayed by CPU saturation, runtime
starvation, allocator pressure, a slow subscriber, an overloaded event loop,
or a queue that grows faster than it is drained. A book can still be marked
`Live` even though the newest applied update is materially older than the
market state at the venue.

The important distinction is between:

1. the timestamp when the exchange generated an update;
2. the timestamp when the network delivered it;
3. the timestamp when the parser accepted it;
4. the timestamp when the book applied it;
5. the timestamp when the solver evaluated it; and
6. the timestamp when the order reached the exchange.

The current freshness gate primarily protects the age of the locally applied
book. It must eventually protect the entire decision-to-order path, not just
the book update.

#### Specific scenario

A live soccer game is tied 0-0 at minute 88. A goal is scored. The true
Polymarket price for the draw moves from 80 cents to 5 cents, while the local
Polymarket event loop is delayed and still exposes the old 80-cent quote.
The Kalshi feed has already processed the corresponding move and shows 5 cents.
The solver compares inconsistent market times and declares a cross-venue edge.

#### Loss mechanism

The engine may submit an order based on a quote that was already invalid when
the decision was made. One leg can fill at the new price while the stale leg
does not fill, fills only partially, or fills at a price that no longer makes
the pair profitable. The resulting position is directional rather than
risk-free.

#### Important order-semantics correction

A buy limit at 20 cents cannot normally fill against a current ask at 95 cents.
If the intended order is a marketable buy with a 95-cent ceiling, the limit
must be expressed at or above the current ask, and the loss analysis must use
that explicit ceiling. The underlying concern remains valid: stale assumptions
can make a supposedly safe order marketable at a price that destroys the
edge. Every scenario must distinguish between:

- a passive limit order;
- a marketable limit order;
- an immediate-or-cancel order;
- a fill-or-kill order; and
- a true market order, if the venue supports one.

#### Required controls

- Carry exchange timestamps and local receive/apply timestamps through the
  decision object.
- Reject a candidate if any leg exceeds the maximum age at evaluation.
- Recheck age immediately before submission.
- Recheck the quote and available depth after the order client is ready to
  send, not only when the solver first observed it.
- Measure event-loop delay and queue depth independently from book age.
- Use bounded channels or explicit drop/resync policies. An unbounded queue
  converts temporary load into increasingly stale trading decisions.
- Treat a sequence gap, disconnect, parser backlog, or stale heartbeat as a
  venue-health failure for every group that depends on that venue.
- Include a monotonic decision deadline in every planned trade. If the deadline
  expires, cancel the plan before placing any leg.

### Scenario B: Asymmetric API round-trip times

#### Vulnerability

Two venues have different network paths, authentication costs, matching
engines, rate limits, acknowledgement semantics, and finality models. Starting
two Tokio futures concurrently does not make their fills atomic or equally
fast.

#### Specific scenario

The engine sees a 3-cent spread on an inflation contract and starts both legs.
Kalshi acknowledges in 15 ms. Polymarket takes 140 ms because of gateway or
signature-verification congestion. During the 125 ms difference, another bot
consumes the required Polymarket liquidity.

#### Loss mechanism

The fast leg fills while the slow leg is rejected, times out, or receives a
smaller fill. The engine now owns an unhedged position. A favorable-looking
cross-venue trade has become a directional bet with an unknown exit price.

#### Required controls

- Define a maximum leg skew, not only a maximum total timeout.
- Record `submitted_at`, `acknowledged_at`, `accepted_at`, `partially_filled_at`,
  and `filled_at` for every order.
- Treat an acknowledgement as different from a fill.
- Treat a cancel acknowledgement as different from a confirmed zero position.
- Use venue-specific pre-flight estimates for authentication and network delay.
- Do not submit a second leg unless the first leg's risk can be covered under
  the configured worst-case hedge model.
- Maintain a per-venue circuit breaker for latency spikes, error bursts, rate
  limits, and reject rates.
- Reject opportunities whose edge is smaller than estimated execution drift,
  fees, and the maximum expected hedge cost.

### Additional data-pipeline concerns

#### Timestamp and clock problems

Wall-clock timestamps can jump because of NTP corrections, VM pauses, laptop
sleep, daylight-saving assumptions, or a misconfigured host clock. Latency
budgets must use a monotonic clock for elapsed time. UTC timestamps are still
needed for audit and settlement, but must not be used as the only latency clock.

#### Snapshot/delta ordering races

A resubscription can produce an old snapshot followed by deltas from a newer
session, or a new snapshot can arrive while old deltas remain queued. Each
venue needs a session identifier, sequence policy, and a rule for discarding
updates from an obsolete connection.

#### Backpressure and starvation

The feed reader, parser, book writer, solver, recorder, dashboard publisher,
and executor must not share an unbounded critical path. Recording or dashboard
serialization must not delay book application. Slow consumers should receive a
sampled state or be disconnected, not hold up trading data.

#### Discovery-cache staleness

Market metadata changes independently of live prices. A cached discovery result
can contain an event that has closed, a changed resolution rule, or a market
that is no longer listed. The cache needs a maximum age, status validation, and
an explicit policy for what happens when the venue cannot be reached and the
cache is stale. A stale cache must never silently become an authorization to
trade.

#### Rate limits and partial universe discovery

Pagination can stop because of rate limits, a cursor loop, an API outage, or a
configured page ceiling. Returning a partial universe is dangerous because it
looks valid while silently omitting constraints. Discovery must either complete
or fail closed with an observable incomplete-universe state.

---

## Category 2: Order-book depth and slippage anomalies

### Scenario C: Phantom top-of-book liquidity

#### Vulnerability

Using only the best ask or best bid ignores quantity at each level. A quote is
not executable inventory. It is only executable for the amount posted at that
price, subject to the order disappearing before the request arrives.

#### Specific scenario

Kalshi shows an ask at 60 cents, but only 50 contracts are available there.
The remaining 1,950 contracts are at 64 cents. The opposing Polymarket leg is
quoted at 38 cents.

#### Loss mechanism

The intended average Kalshi fill is approximately 63.9 cents, not 60 cents.
The combined cost is approximately 101.9 cents, before fees and transfer costs,
for a position whose guaranteed payoff is at most 100 cents. A naive order
router locks in a loss by sweeping the book.

#### Required controls

- Walk all relevant price levels before submission.
- Compute both worst-case total cost and volume-weighted average cost.
- Include venue fees, rebates, gas, settlement charges, and transfer costs.
- Cap quantity at the thinnest executable leg.
- Use a price ceiling for every aggressive order.
- Revalidate depth immediately before sending and treat the quote as expired
  after the configured age budget.
- Reject if the worst-case all-in cost is not below the guaranteed payoff by a
  configured safety margin.
- Preserve the exact depth snapshot used for the decision in the audit record.

### Scenario D: Toxic order-routing loop

#### Vulnerability

Execution logic may repeatedly cross the spread to force fills. This can be
appropriate for emergency risk reduction, but it is not a neutral operation.
Every crossing pays spread, taker fees, and adverse selection. In a volatile
market, a loop can repeatedly buy high and sell low while the strategy appears
to be “maintaining” a hedge.

#### Specific scenario

The engine has a breaking position during a news event. The book widens to a
45-cent bid and 55-cent ask. A recovery loop repeatedly crosses the 10-cent
spread in small bursts to rebalance.

#### Loss mechanism

The position may be directionally correct and still lose money because spread
attrition, fees, price impact, and repeated cancellation/replacement costs
consume the account.

#### Required controls

- Separate normal entry, passive rebalancing, aggressive hedge, and emergency
  liquidation modes.
- Put a hard monetary and quantity budget on rescue execution.
- Put a maximum number of retries on each recovery path.
- Stop retrying when the expected rescue loss exceeds the configured maximum.
- Prefer one explicit emergency order over an uncontrolled loop.
- Emit a high-priority alert when the strategy enters panic-cover mode.
- Require operator acknowledgement before continuing after the circuit breaker.

### Additional order-book concerns

#### Partial depth and stale levels

A level may be removed after a snapshot but before an order is accepted. The
engine must model disappearing liquidity as normal, not exceptional. A plan
that requires every level to remain unchanged is too brittle; a plan that
accepts arbitrary degradation is unsafe.

#### Hidden, replenishing, and non-displayed liquidity

Displayed depth is not always the full depth, and observed depth can be
replenished or canceled. The engine should use conservative displayed-depth
assumptions and must not count expected hidden liquidity toward guaranteed
quantity.

#### Tick-size, lot-size, and unit mismatches

Kalshi cents, Polymarket token quantities, USDC decimal units, and venue-specific
minimums must be normalized before solving. Every conversion needs explicit
rounding direction. Quantity rounding must never increase the intended risk or
make a safe price appear cheaper than it is.

#### Fees and rebates changing at execution time

Fee schedules can vary by venue, side, tier, maker/taker status, or market.
The solver must use the fee schedule applicable to the order actually sent. A
candidate that is profitable only under a stale fee table is not safe.

#### Price improvement and worse-than-limit fills

The fill model must confirm that a venue never reports a fill outside the
submitted limit. A venue bug, adapter bug, or incorrect side conversion here
can turn a supposedly protected order into an unbounded order.

---

## Category 3: Semantic, structural, and settlement divergence

### Scenario E: Settlement-mechanism mismatch

#### Vulnerability

Matching contracts by title, ticker fragments, or embedding similarity is not
enough. A cross-venue equivalence requires identical:

- underlying event;
- observation window;
- cutoff timestamp and timezone;
- resolution source;
- resolution authority;
- tie and rounding rules;
- treatment of missing or revised data;
- cancellation and void rules; and
- payout polarity.

#### Specific scenario

Both venues appear to list “Will the Federal Reserve cut rates in September?”
Kalshi settles from the official FOMC statement on September 17. Polymarket
settles from an UMA resolution based on an effective federal-funds-rate value
published on October 1. The titles match, but the contracts do not represent
the same claim.

#### Loss mechanism

The pair is not a guaranteed hedge. One venue can resolve YES while the other
resolves NO, or either venue can void or delay the market under different rules.
The engine can lose the intended capital, incur an expensive unwind, or hold
one leg until a settlement date it did not model.

#### Important payoff correction

Whether opposite legs lose depends on the exact payout mapping. If the engine
buys YES on one venue and NO on the other, divergent outcomes can sometimes
make both legs pay, while matching outcomes may leave only one leg paying. The
real problem is not that every semantic mismatch loses 100%; it is that the
position no longer has a mathematically guaranteed payoff. The solver must not
call it arbitrage until the settlement relation is verified.

#### Required controls

- Store a structured resolution manifest for every cross-venue pair.
- Require human verification before an equivalent relation can be live-traded.
- Hash the resolution rules and retain the source URLs/documents.
- Compare resolution timestamps, not just dates in titles.
- Explicitly model YES/NO polarity and venue-specific order-side semantics.
- Treat changed rules as a new contract and invalidate existing equivalence.
- Block trading when either venue's rules cannot be fetched or verified.
- Test each relation against every possible pair of venue resolutions.

### Scenario F: regulatory and capital-freezing hazard

#### Vulnerability

Kalshi USD balances and Polymarket crypto balances are not interchangeable.
Moving capital between a bank rail and a blockchain wallet can take minutes,
hours, or business days. A strategy that assumes instant cross-venue funding
has an implicit and potentially enormous financing assumption.

#### Specific scenario

The engine deploys nearly all available capital on one venue. A new opportunity
requires capital on the other venue, but the transfer is delayed by ACH
settlement, banking hours, compliance review, blockchain congestion, a bridge,
or a wallet policy.

#### Loss mechanism

The engine cannot complete or unwind a hedge. Existing inventory remains
directional, and the supposedly available capital is not operationally
available. A dashboard showing total balances can be dangerously misleading if
it does not distinguish free, locked, withdrawable, transferable, and
settled-final balances.

#### Required controls

- Maintain separate available, reserved, pending, and withdrawable balances per
  venue and asset.
- Never count a balance as cross-venue deployable until it is confirmed usable
  by the target venue.
- Set venue-specific capital reserves and maximum utilization percentages.
- Model transfer time and transfer fees as explicit risk inputs.
- Add stablecoin depeg, chain outage, wallet compromise, and withdrawal pause
  scenarios.
- Stop opening new positions when the hedge venue's free balance falls below
  its reserve threshold.
- Make the risk engine understand settlement currency and collateral type.

### Additional semantic and structural concerns

#### Conditional and correlated markets

Two markets may look related but have conditional dependencies, shared oracle
failure modes, or overlapping outcomes. A ladder, exhaustive set, implication,
and complement are different relations and must not be inferred from naming
patterns alone.

#### Multi-outcome completeness

An exhaustive group is safe only when the listed members truly cover every
possible outcome. Missing “other,” cancellation, tie, or void outcomes can
break the 100-cent payoff assumption.

#### Market lifecycle transitions

Open, paused, closed, determined, settled, canceled, and finalized are not
interchangeable. The registry and executor need explicit transitions and must
not place orders in a market that is technically listed but no longer
tradable.

#### Resolution-source availability

An official source can be delayed, revised, inaccessible, or contradicted by a
venue oracle. The strategy needs a policy for delayed resolution and source
disagreement rather than assuming settlement is immediate and infallible.

#### Polarity and complement mistakes

“Buy NO” can be represented differently across venues. On Kalshi, one wire-side
book can economically represent the complement of another outcome. On another
venue, YES/NO may be separate token identifiers. Every adapter must expose a
canonical outcome and retain the original venue-side representation for audit.

---

## Category 4: Execution atomicity and recovery

### There is no true cross-venue atomic transaction

`tokio::join!`, `tokio::spawn`, and sending two requests at the same time only
reduce scheduling delay. They do not make two exchange APIs commit together.
One venue can accept while the other rejects, one can fill partially, or one
can accept an order while its acknowledgement is lost.

The system therefore needs an explicit state machine:

```text
PLANNED
  -> PRECHECKED
  -> SUBMITTING
  -> PARTIAL / FILLED / REJECTED / UNKNOWN
  -> HEDGED
  -> UNWINDING
  -> RECONCILIATION_REQUIRED
  -> CLOSED
```

Every transition must be persisted with a timestamp, venue response, client
order ID, and retry decision. “The future timed out” is not a final exchange
state.

### Strict atomic timeout requirement

Wrap each leg and the overall trade window in `tokio::time::timeout`, but do
not treat timeout as cancellation. A timeout only stops waiting locally. The
remote order may still be live.

Required behavior:

1. Send explicit limit orders with a bounded price.
2. Wait only for the configured acknowledgement deadline.
3. If the deadline expires, query the venue by client order ID.
4. Cancel any open order.
5. Poll until the venue confirms canceled, filled, or otherwise terminal.
6. Reconcile actual fills before deciding whether to hedge or unwind.
7. Escalate `UNKNOWN` to an operator-visible state; never silently retry.

### Scenario G: Leg A fills, Leg B fails

If Kalshi fills and Polymarket fails, the engine owns a Kalshi position. It must
not assume that a panic sell will get the original price. The recovery path
must calculate the current exit cost, available depth, fees, and maximum loss.

Possible policies, selected by configuration and market type:

- immediately cross the spread to flatten;
- submit a bounded hedge at a less aggressive price and wait briefly;
- hedge with a correlated instrument only if correlation is explicitly modeled;
- hold temporarily under a separately authorized inventory limit; or
- halt all new trading and require manual reconciliation.

The default for an unfamiliar cross-venue instrument should be fail-closed,
not “keep trying until filled.”

### Recovery hazards

- An unwind can be slower than the original fill.
- The original order can fill after the engine starts unwinding.
- A retry can duplicate the position if the first request succeeded but the
  acknowledgement was lost.
- The hedge venue can reject the recovery order because the market closed.
- A price ceiling can prevent a fill exactly when liquidity is needed most.
- Repeated recovery attempts can amplify losses.
- A process crash can leave open orders without a local state record.

### Required execution features

- Globally unique, durable client order IDs.
- Idempotent submission and query-by-client-ID reconciliation.
- Venue adapters with explicit order states, not only `Result<Fill>`.
- Persistent trade intents before network submission.
- A startup reconciliation pass that queries all recent open orders and fills.
- A kill switch that stops new orders while allowing cancellation/reconciliation.
- A panic-cover budget per event, venue, and calendar day.
- A dead-letter queue for unresolved order states.
- Alerts for every position that is not fully hedged within its deadline.

---

## Category 5: Capital, risk, and portfolio hazards

### Balance is not buying power

The risk engine must distinguish:

- cash balance;
- settled balance;
- free trading balance;
- locked collateral;
- pending deposits;
- pending withdrawals;
- reserved capital for open positions;
- capital reserved for emergency hedges; and
- capital that is present but operationally unavailable.

The maximum trade size must use the minimum of all relevant constraints, not
only a single account balance.

### Correlated-event concentration

Many markets can fail together: all inflation markets, all election markets,
all sports markets for one match, or all contracts depending on one oracle.
Per-market limits are insufficient. Risk limits should be keyed by event,
theme, resolution source, oracle, venue, and settlement date.

### Adverse movement before settlement

Even a theoretically hedged pair can produce mark-to-market losses, margin
requirements, liquidation pressure, or a temporary capital shortfall. The
portfolio model must stress the path to settlement, not only the final payoff.

### Fees, gas, and transfer costs

Small edges disappear under taker fees, gas, withdrawal fees, conversion
spreads, bridge fees, and failed-order costs. Costs must be attached to the
trade and the recovery path, not hidden in a global average.

### Operational and counterparty risk

The system must account for API outages, exchange maintenance, withdrawal
pauses, account restrictions, key revocation, rate-limit bans, oracle disputes,
chain reorganizations, and venue insolvency or legal intervention. These are
not solver edge cases; they are position-risk inputs.

---

## Category 6: Rust and asynchronous-runtime hazards

### Cancellation is not rollback

Dropping a Tokio future does not undo a request already received by an
exchange. The executor must reconcile remote state after local cancellation or
timeout.

### Task leaks and duplicate workers

Reconnect loops, order monitors, and recovery tasks need ownership and
shutdown guarantees. A reconnect bug can create two feed readers, duplicate
events, duplicate orders, or competing writers to the same book.

### Shared-state visibility

The feed, solver, executor, portfolio, recorder, and dashboard must have clear
ownership boundaries. A stale cloned snapshot or an unsynchronized cache can
make one component act on state another component has already invalidated.

### Numeric correctness

Prices, quantities, fees, gas, and token decimals must use checked integer or
fixed-point arithmetic. Audit every conversion for overflow, negative values,
rounding direction, and maximum quantity. A single floating-point conversion in
the money path can create false edge or understate fees.

### Persistence and crash recovery

The process can crash between sending an order and recording its result. A
durable intent log and startup reconciliation are mandatory before live
cross-venue execution.

### Secrets and logs

Private keys, API keys, signatures, wallet addresses, and full order payloads
must not leak through debug logs, panic messages, telemetry, crash dumps, or
recorded fixtures. Production logs should include correlation IDs and safe
metadata, not credentials.

---

## Category 7: Monitoring, testing, and operational readiness

### Metrics that must exist

- feed receive, parse, apply, and solver latency;
- queue depth and dropped-message counts;
- book age by venue and contract;
- sequence gaps and resync duration;
- quote-to-submit age;
- submit-to-ack and submit-to-fill latency;
- reject, cancel, partial-fill, and unknown-order rates;
- realized and unrealized P&L;
- fees, gas, transfers, and rescue losses;
- open unhedged exposure;
- available versus locked capital;
- discovery completeness and cache age; and
- time spent in circuit-breaker states.

### Testing plan

Before production cross-venue trading, add:

1. deterministic replay with injected queue delays;
2. out-of-order, duplicated, missing, and delayed WebSocket messages;
3. snapshot/delta session rollover tests;
4. stale-data decision and stale-data-at-submit tests;
5. deep-book slippage and disappearing-liquidity tests;
6. partial-fill matrices for every leg ordering;
7. lost acknowledgement and duplicate-submit tests;
8. cancellation races where an order fills during cancellation;
9. settlement-manifest mismatch tests;
10. unavailable-balance and delayed-transfer tests;
11. API rate-limit, timeout, and server-error tests;
12. process-crash recovery tests between every order-state transition;
13. property tests proving that accepted trades satisfy the payoff guarantee;
14. fuzz tests for venue JSON, numeric fields, and order states; and
15. a long-running soak test under CPU and network pressure.

### Deployment plan

Use progressive exposure:

- offline replay;
- live market-data shadow mode;
- paper execution against live books;
- tiny canary quantity with a hard daily loss limit;
- one venue and one manually verified relation;
- limited hours and manually monitored operation; then
- gradual expansion by venue, event class, and capital allocation.

Do not enable auto-discovered inferred groups for live orders merely because
they pass structural validation. Structural validation proves that the data is
internally consistent; it does not prove that the resolution interpretation is
correct.

---

## Production-grade safeguarding checklist

The intended execution path should look like this:

```text
[ INCOMING MARKET DATA STREAM ]
              |
  1. Validate sequence/session and timestamps
              |
  2. Drop or resync if data is too old or incomplete
              |
  3. Build a depth-aware executable quote
              |
  4. Validate canonical contract and settlement manifest
              |
  5. Check worst-case payoff > fees + slippage + recovery margin
              |
        +-----+---------------------+
        |                           |
[ Leg A: Kalshi ]             [ Leg B: Polymarket ]
- explicit limit price       - explicit limit price
- bounded quantity            - bounded quantity
- FOK/IOC as supported        - FOK/IOC as supported
- timeout + query              - timeout + query
- durable client ID            - durable client ID
        |                           |
        +-------------+-------------+
                      |
  6. Reconcile every remote order state
                      |
  7. If one leg is exposed, enter bounded panic-cover mode
                      |
  8. Cancel, hedge, unwind, or escalate to reconciliation
```

### Explicit safety rules

#### Rule 1: Stale-data gate

Reject a trade if any required market update exceeds the configured age or if
the decision-to-submit deadline has expired. The age budget must be configurable
per venue and market class; a single universal 10 ms rule is not realistic for
all environments, while a 500 ms default may be far too permissive for a fast
news market.

#### Rule 2: Depth-aware sweeper

Calculate the full quantity-weighted cost of every leg, including fees and
rounding. Never multiply the L1 price by the requested quantity unless the
depth snapshot proves that quantity exists at that price.

#### Rule 3: Pre-flight boundary validation

Require:

```text
guaranteed payoff
  > worst-case entry cost
  + fees
  + gas/transfer costs
  + estimated recovery cost
  + configured safety margin
```

If any term is unknown, the candidate is not a guaranteed arbitrage candidate.

#### Rule 4: Explicit order semantics

Use explicit limit prices and venue-supported time-in-force controls. Never
silently downgrade a protected order into a market order. Record whether each
order is maker, taker, post-only, IOC, FOK, or another venue-specific type.

#### Rule 5: Strict timeout plus remote reconciliation

Wrap local waits in `tokio::time::timeout`, but after a timeout query and cancel
the remote order. A local timeout must transition to `UNKNOWN`, not `NO_FILL`.

#### Rule 6: Panic-cover circuit breaker

If one leg fills and the other fails, stop new risk, cancel all related open
orders, calculate the available hedge/unwind options, and execute only within a
pre-authorized rescue budget. If the position cannot be safely closed, create a
reconciliation incident immediately.

#### Rule 7: Human-verifiable semantics

Do not trade an equivalent cross-venue group until both resolution manifests
have been reviewed and hashed. Titles and model-generated similarity are useful
for discovery, never sufficient for authorization.

#### Rule 8: Capital isolation

Keep reserves for fees, rescue trades, and operational withdrawals. A strategy
must not deploy all capital just because the solver sees available notional.

---

## Prioritized work items

### P0 — Must exist before cross-venue live orders

- Build a canonical settlement manifest and human-verification workflow.
- Add durable order intents and startup order reconciliation.
- Model explicit remote order states, including `UNKNOWN`.
- Implement one-leg-filled/one-leg-failed recovery with bounded loss.
- Add per-venue balances, reserves, and unhedged exposure limits.
- Add decision-age and submit-age gates for every leg.
- Add end-to-end partial-fill, cancel-race, and lost-acknowledgement tests.
- Keep inferred relations blocked from live trading by default.

### P1 — Must exist before meaningful paper-to-live scale

- Implement Polymarket depth and execution adapters with correct decimal and
  token semantics.
- Add venue-specific fee, gas, transfer, and settlement-cost models.
- Add metrics, alerts, dashboards, and kill-switch controls.
- Add delayed-data, rate-limit, cursor-completeness, and reconnect chaos tests.
- Add capital transfer and stablecoin/fiat availability modeling.
- Add a shadow-mode comparison between local decisions and executable fills.

### P2 — Required for robustness and expansion

- Add per-event and per-oracle correlation limits.
- Add formal resolution-rule versioning and invalidation on metadata changes.
- Add fuzzing and long-running soak tests.
- Add canary deployment tooling and automated rollback.
- Add audit exports sufficient to reconstruct every decision and order state.
- Add operator runbooks for exchange outages, unknown orders, oracle disputes,
  and stuck transfers.

---

## Open design decisions

These questions should be answered before implementation of the corresponding
work items:

1. What is the maximum tolerated quote age for each venue and market class?
2. What maximum leg-skew and total execution deadline is acceptable?
3. Which venue-specific order types provide the strongest fill protection?
4. What is the maximum permitted panic-cover loss per trade, event, venue, and
   day?
5. Should an unmatched leg always be flattened, or may a separately authorized
   inventory strategy hold it temporarily?
6. What evidence is required to mark two resolution manifests as equivalent?
7. How much capital must remain reserved for emergency hedging and fees?
8. Which states require automatic shutdown versus manual acknowledgement?
9. What is the source of truth when venue metadata and settlement results
   disagree?
10. What audit retention period is required for market data, decisions, orders,
    fills, cancellations, and recovery actions?

### Immediate implementation questions

- Should the next Rust change introduce an `OrderBookSweeper` abstraction that
  returns worst-case cost, average fill, consumed levels, and an expiry time?
- Should the next execution change introduce a Tokio panic-cover state machine
  with explicit cancel/query/reconcile phases?
- Should cross-venue equivalence be represented as a separate verified registry
  artifact rather than inferred directly from live market metadata?

Until these controls exist and are tested under failure, Sum100 should remain in
replay, shadow, or paper mode for cross-venue strategies.
