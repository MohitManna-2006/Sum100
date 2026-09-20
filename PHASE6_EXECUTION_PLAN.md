# Phase 6: Atomic Execution and Portfolio Management

**Status:** Design (not yet scheduled in PLAN.md)
**Depends on:** Phases 0–5 (registry, engine, solver)
**Supercedes:** Paper executor (src/exec.rs) with real order placement
**Goal:** Eliminate manual trading gates by automating health checks, capital constraints, risk limits, and atomic cross-venue order execution at low latency.

---

## 1. Overview

Phase 5 builds the registry and engine loop. Phase 6 replaces the paper executor with a real one and adds portfolio management infrastructure. After phase 6, the engine runs fully automated: it detects opportunities, verifies them against registry, checks health/capital/risk, places both legs atomically with 500ms timeout, tracks positions, and broadcasts fills to the UI. No human intervention except initial verification of contract pairs.

---

## 2. Components and Latency Budget

| Component | Latency | Dependency | Notes |
|-----------|---------|------------|-------|
| Health monitor | < 1µs | Feed instrumentation | Per-venue heartbeat, block if stale |
| Portfolio tracker | < 1ms | Position DB | Track open positions, capital, daily PnL |
| Risk engine | 1ms | Registry themes/events | Check single-theme, single-event, sector limits |
| Atomic executor | 50–200ms | Kalshi + Polymarket API keys | Fire both orders in parallel, cancel loser |
| Portfolio optimizer | 100ms | Knapsack sort | Rank opportunities by return/capital ratio |
| **Total tick-to-trade** | **~250ms** | All above | From book update to both orders placed |

---

## 3. Phase 6a: Real Executor and Order Placement

### 3.1 Order Client Traits

```rust
// src/executor/mod.rs

#[async_trait]
pub trait OrderClient: Send + Sync {
    async fn place_order(
        &self,
        contract_id: ContractId,
        side: Side,
        price_cents: i64,
        size: i64,
        timeout_ms: u64,
    ) -> Result<OrderId, OrderError>;

    async fn cancel_order(&self, order_id: OrderId) -> Result<(), OrderError>;
    async fn get_order_status(&self, order_id: OrderId) -> Result<OrderStatus, OrderError>;
}

pub enum OrderStatus {
    Pending,
    PartiallyFilled { filled: i64 },
    Filled { filled: i64 },
    Cancelled,
    Failed(String),
}

pub enum OrderError {
    Timeout,
    InsufficientLiquidity,
    InvalidPrice,
    VenueError(String),
    Cancelled(String),
}
```

### 3.2 Atomic Executor

```rust
// src/executor/atomic.rs

pub struct AtomicExecutor {
    kalshi: Arc<dyn OrderClient>,
    polymarket: Arc<dyn OrderClient>,
    config: ExecutorConfig,
}

#[derive(Clone)]
pub struct ExecutorConfig {
    pub timeout_ms: u64,           // 500
    pub max_slippage_cents: i64,   // cancel if one leg gets >5c worse
    pub retry_attempts: u32,       // 1
}

pub struct ExecutedTrade {
    pub opportunity_id: String,
    pub legs: Vec<ExecutedLeg>,
    pub total_cost_cents: i64,
    pub timestamp_ms: u64,
}

pub struct ExecutedLeg {
    pub venue: Venue,
    pub contract_id: ContractId,
    pub side: Side,
    pub price_cents: i64,
    pub size: i64,
    pub order_id: OrderId,
    pub status: OrderStatus,
}

impl AtomicExecutor {
    pub async fn execute(
        &self,
        opportunity: &Opportunity,
    ) -> Result<ExecutedTrade, ExecutorError> {
        let legs = opportunity.legs.clone();
        
        // Fire both legs in parallel
        let start_ms = now_ms();
        let deadline = Duration::from_millis(self.config.timeout_ms);
        
        let results = tokio::time::timeout(
            deadline,
            tokio::join!(
                self.place_leg(&legs[0]),
                self.place_leg(&legs[1]),
            ),
        ).await;

        match results {
            Ok((Ok((order_id_1, price_1)), Ok((order_id_2, price_2)))) => {
                // Both filled: success
                Ok(ExecutedTrade {
                    opportunity_id: opportunity.id.clone(),
                    legs: vec![
                        ExecutedLeg {
                            venue: legs[0].venue,
                            contract_id: legs[0].contract_id,
                            side: legs[0].side,
                            price_cents: price_1,
                            size: legs[0].size,
                            order_id: order_id_1,
                            status: OrderStatus::Filled { filled: legs[0].size },
                        },
                        ExecutedLeg {
                            venue: legs[1].venue,
                            contract_id: legs[1].contract_id,
                            side: legs[1].side,
                            price_cents: price_2,
                            size: legs[1].size,
                            order_id: order_id_2,
                            status: OrderStatus::Filled { filled: legs[1].size },
                        },
                    ],
                    total_cost_cents: opportunity.cost_cents,
                    timestamp_ms: start_ms,
                })
            }
            Ok((Ok((order_id_1, _)), Err(e2))) => {
                // Leg 1 succeeded, leg 2 failed: cancel leg 1
                self.kalshi.cancel_order(order_id_1).await.ok();
                Err(ExecutorError::LeggingFailed {
                    filled_leg: 0,
                    reason: format!("leg 2 failed: {}", e2),
                })
            }
            Ok((Err(e1), Ok((order_id_2, _)))) => {
                // Leg 1 failed, leg 2 succeeded: cancel leg 2
                self.polymarket.cancel_order(order_id_2).await.ok();
                Err(ExecutorError::LeggingFailed {
                    filled_leg: 1,
                    reason: format!("leg 1 failed: {}", e1),
                })
            }
            Ok((Err(e1), Err(e2))) => {
                // Both failed
                Err(ExecutorError::BothFailed {
                    leg1: e1.to_string(),
                    leg2: e2.to_string(),
                })
            }
            Err(_) => {
                // Timeout: cancel both
                Err(ExecutorError::Timeout)
            }
        }
    }

    async fn place_leg(&self, leg: &OpportunityLeg) -> Result<(OrderId, i64), OrderError> {
        let client = match leg.venue {
            Venue::Kalshi => &self.kalshi,
            Venue::Polymarket => &self.polymarket,
        };

        client.place_order(
            leg.contract_id,
            leg.side,
            leg.price_cents,
            leg.size,
            self.config.timeout_ms / 2,  // Half budget per leg
        ).await.map(|order_id| (order_id, leg.price_cents))
    }
}

pub enum ExecutorError {
    LeggingFailed { filled_leg: u32, reason: String },
    BothFailed { leg1: String, leg2: String },
    Timeout,
}
```

### 3.3 Kalshi and Polymarket Clients

```rust
// src/executor/kalshi_client.rs

pub struct KalshiOrderClient {
    api_key_id: String,
    private_key: RsaPrivateKey,
    http_client: reqwest::Client,
}

#[async_trait]
impl OrderClient for KalshiOrderClient {
    async fn place_order(
        &self,
        contract_id: ContractId,
        side: Side,
        price_cents: i64,
        size: i64,
        timeout_ms: u64,
    ) -> Result<OrderId, OrderError> {
        let ticker = contract_id.ticker();  // Convert ContractId to ticker
        let yes_price = match side {
            Side::Yes => price_cents as f64 / 100.0,
            Side::No => 100.0 - (price_cents as f64 / 100.0),
        };

        let body = json!({
            "ticker": ticker,
            "side": match side { Side::Yes => "yes", Side::No => "no" },
            "action": "buy",
            "quantity": size,
            "yes_price": yes_price,
        });

        let response = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.http_client.post("https://api.elections.kalshi.com/trade-api/v2/orders")
                .json(&body)
                .send(),
        ).await??;

        let result: serde_json::Value = response.json().await?;
        let order_id = result["order_id"]
            .as_str()
            .ok_or(OrderError::VenueError("no order_id in response".into()))?
            .to_string();

        Ok(OrderId(order_id))
    }

    async fn cancel_order(&self, order_id: OrderId) -> Result<(), OrderError> {
        self.http_client
            .delete(format!(
                "https://api.elections.kalshi.com/trade-api/v2/orders/{}",
                order_id.0
            ))
            .send()
            .await?;
        Ok(())
    }

    async fn get_order_status(&self, order_id: OrderId) -> Result<OrderStatus, OrderError> {
        let response = self.http_client
            .get(format!(
                "https://api.elections.kalshi.com/trade-api/v2/orders/{}",
                order_id.0
            ))
            .send()
            .await?;

        let result: serde_json::Value = response.json().await?;
        let status = result["status"].as_str().unwrap_or("unknown");

        Ok(match status {
            "filled" => OrderStatus::Filled {
                filled: result["quantity_matched"].as_i64().unwrap_or(0),
            },
            "cancelled" => OrderStatus::Cancelled,
            _ => OrderStatus::Pending,
        })
    }
}

// src/executor/polymarket_client.rs (similar structure)
```

---

## 4. Phase 6b: Portfolio Tracker

### 4.1 Position State

```rust
// src/portfolio/mod.rs

pub struct Position {
    pub id: PositionId,
    pub event_id: EventId,
    pub contracts: Vec<PositionLeg>,
    pub open_at_ms: u64,
    pub cost_cents: i64,
    pub open_pnl_cents: i64,  // Mark-to-market
    pub status: PositionStatus,
}

pub struct PositionLeg {
    pub contract_id: ContractId,
    pub venue: Venue,
    pub side: Side,
    pub size: i64,
    pub average_price_cents: i64,
    pub order_ids: Vec<OrderId>,
}

pub enum PositionStatus {
    Open,
    PartiallyFilled { filled_contracts: i64, pending: i64 },
    Closed { realized_pnl_cents: i64, closed_at_ms: u64 },
}

pub struct Portfolio {
    pub positions: Vec<Position>,
    pub available_capital_cents: i64,
    pub realized_pnl_cents: i64,
    pub max_position_size: i64,
    pub daily_loss_limit_cents: i64,
    pub daily_loss_used_cents: i64,
    pub last_reset_ms: u64,
}

impl Portfolio {
    pub fn can_afford(&self, trade: &Opportunity) -> bool {
        trade.cost_cents <= self.available_capital_cents
    }

    pub fn can_accept_pnl(&self) -> bool {
        (self.daily_loss_used_cents + 100).abs() <= self.daily_loss_limit_cents
    }

    pub fn add_position(&mut self, trade: &ExecutedTrade) {
        let position = Position {
            id: PositionId::new(),
            event_id: /* from opportunity */,
            contracts: trade.legs.iter().map(|leg| PositionLeg {
                contract_id: leg.contract_id,
                venue: leg.venue,
                side: leg.side,
                size: leg.size,
                average_price_cents: leg.price_cents,
                order_ids: vec![leg.order_id.clone()],
            }).collect(),
            open_at_ms: trade.timestamp_ms,
            cost_cents: trade.total_cost_cents,
            open_pnl_cents: 0,  // Will be updated by market data
            status: PositionStatus::Open,
        };

        self.available_capital_cents -= trade.total_cost_cents;
        self.positions.push(position);
    }

    pub fn close_position(&mut self, position_id: PositionId, realized_pnl: i64) {
        if let Some(pos) = self.positions.iter_mut().find(|p| p.id == position_id) {
            pos.status = PositionStatus::Closed {
                realized_pnl_cents: realized_pnl,
                closed_at_ms: now_ms(),
            };
            self.available_capital_cents += pos.cost_cents;
            self.realized_pnl_cents += realized_pnl;
            self.daily_loss_used_cents += realized_pnl;
        }
    }

    pub fn update_mark_to_market(&mut self, contract_id: ContractId, new_price_cents: i64) {
        for pos in &mut self.positions {
            for leg in &mut pos.contracts {
                if leg.contract_id == contract_id {
                    let current_value = leg.size * new_price_cents / 100;
                    let cost_basis = leg.size * leg.average_price_cents / 100;
                    pos.open_pnl_cents = match leg.side {
                        Side::Yes => current_value - cost_basis,
                        Side::No => cost_basis - current_value,
                    };
                }
            }
        }
    }
}
```

---

## 5. Phase 6c: Venue Health Monitor

### 5.1 Health Tracking

```rust
// src/health.rs

pub struct VenueHealth {
    pub venue: Venue,
    pub last_message_ms: u64,
    pub consecutive_errors: u32,
    pub is_alive: bool,
    pub idle_timeout_ms: u64,  // 5000
    pub error_threshold: u32,   // 3
}

impl VenueHealth {
    pub fn check(&mut self, now_ms: u64) -> bool {
        if now_ms - self.last_message_ms > self.idle_timeout_ms {
            self.consecutive_errors += 1;
            if self.consecutive_errors > self.error_threshold {
                self.is_alive = false;
            }
        } else {
            self.consecutive_errors = 0;  // Reset on successful message
            self.is_alive = true;
        }
        self.is_alive
    }

    pub fn record_message(&mut self, now_ms: u64) {
        self.last_message_ms = now_ms;
        self.consecutive_errors = 0;
        self.is_alive = true;
    }
}

pub struct HealthMonitor {
    pub kalshi: VenueHealth,
    pub polymarket: VenueHealth,
}

impl HealthMonitor {
    pub fn can_trade(&mut self, now_ms: u64) -> bool {
        let kalshi_ok = self.kalshi.check(now_ms);
        let poly_ok = self.polymarket.check(now_ms);
        kalshi_ok && poly_ok
    }
}
```

---

## 6. Phase 6d: Risk Engine

### 6.1 Exposure Limits

```rust
// src/risk/mod.rs

pub struct RiskLimits {
    pub max_single_theme_cents: i64,      // $5k = 500_000 cents
    pub max_single_event_cents: i64,      // $10k
    pub daily_loss_limit_cents: i64,      // $1k
    pub max_concurrent_trades: usize,     // 20
}

pub struct RiskEngine {
    pub limits: RiskLimits,
    pub registry: Arc<Registry>,
}

impl RiskEngine {
    pub fn can_add_trade(&self, portfolio: &Portfolio, trade: &Opportunity) -> bool {
        // Check 1: Theme exposure
        let theme = self.registry.theme(trade.group_id);
        let theme_exposure: i64 = portfolio
            .positions
            .iter()
            .filter(|p| self.registry.theme(p.event_id) == theme)
            .map(|p| p.cost_cents)
            .sum();

        if theme_exposure + trade.cost_cents > self.limits.max_single_theme_cents {
            return false;
        }

        // Check 2: Event exposure
        let event = self.registry.event(trade.group_id);
        let event_exposure: i64 = portfolio
            .positions
            .iter()
            .filter(|p| p.event_id == event)
            .map(|p| p.cost_cents)
            .sum();

        if event_exposure + trade.cost_cents > self.limits.max_single_event_cents {
            return false;
        }

        // Check 3: Daily loss limit
        if !portfolio.can_accept_pnl() {
            return false;
        }

        // Check 4: Concurrent trade count
        if portfolio.positions.len() >= self.limits.max_concurrent_trades {
            return false;
        }

        true
    }
}
```

---

## 7. Phase 6e: Portfolio Optimizer (Optional)

### 7.1 Knapsack Ranking

```rust
// src/portfolio/optimizer.rs

pub fn rank_opportunities_by_efficiency(
    opportunities: &[Opportunity],
) -> Vec<&Opportunity> {
    let mut ranked = opportunities.to_vec();
    ranked.sort_by(|a, b| {
        // Sort by return/capital ratio, descending
        let ratio_a = (a.annualized_return * 10000) / (a.cost_cents.max(1) as f64);
        let ratio_b = (b.annualized_return * 10000) / (b.cost_cents.max(1) as f64);
        ratio_b.partial_cmp(&ratio_a).unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked
}

pub fn select_trades_within_capital(
    ranked: &[&Opportunity],
    available_capital: i64,
) -> Vec<&Opportunity> {
    let mut selected = Vec::new();
    let mut capital_used = 0;

    for opp in ranked {
        if capital_used + opp.cost_cents <= available_capital {
            capital_used += opp.cost_cents;
            selected.push(*opp);
        }
    }

    selected
}
```

---

## 8. Integration: Engine Loop (Phase 5, Updated)

```rust
// src/engine.rs (updated)

pub async fn run_engine(
    mut feed: Box<dyn Feed>,
    registry: Arc<Registry>,
    config: EngineConfig,
) {
    let mut book_store = BookStore::new(Venue::Kalshi, &[], clock.clone());
    let mut solver = Solver::new(&registry, &config.solver);
    let executor = AtomicExecutor::new(kalshi_client, polymarket_client, executor_config);
    let mut portfolio = Portfolio::new(config.starting_capital, config.daily_loss_limit);
    let mut health_monitor = HealthMonitor::new();
    let risk_engine = RiskEngine::new(config.risk_limits, registry.clone());

    let api_state = Arc::new(Mutex::new(EngineState::default()));

    while let Some(event) = feed.next().await {
        // Apply book update
        book_store.apply(event);

        // Check health before continuing
        if !health_monitor.can_trade(now_ms()) {
            continue;  // Skip trades until both venues are healthy
        }

        // Solve
        let opportunities = solver.solve(book_store.dirty_set());

        // Filter and rank
        let tradeable: Vec<_> = opportunities
            .iter()
            .filter(|opp| {
                registry.verified(opp.group_id) &&
                portfolio.can_afford(opp) &&
                risk_engine.can_add_trade(&portfolio, opp)
            })
            .collect();

        let ranked = rank_opportunities_by_efficiency(&tradeable);
        let selected = select_trades_within_capital(&ranked, portfolio.available_capital_cents);

        // Execute trades
        for trade in selected {
            match executor.execute(trade).await {
                Ok(executed) => {
                    portfolio.add_position(&executed);
                    // Broadcast ExecutedTrade event
                }
                Err(e) => {
                    // Log legging failure, increment metric
                    metrics.legging_failures.inc();
                }
            }
        }

        // Update portfolio mark-to-market and broadcast EngineState
        for book in book_store.books() {
            portfolio.update_mark_to_market(book.contract_id, book.best_bid_cents);
        }

        let state = EngineState {
            timestamp_ms: now_ms(),
            opportunities: tradeable.len(),
            positions_open: portfolio.positions.len(),
            capital_deployed: portfolio.deployed_capital(),
            daily_pnl: portfolio.realized_pnl_cents,
            health: HealthState {
                kalshi_alive: health_monitor.kalshi.is_alive,
                polymarket_alive: health_monitor.polymarket.is_alive,
            },
        };

        api_state_broadcast.send(state).ok();
    }
}
```

---

## 9. Exit Criteria

| Criterion | Done when |
|-----------|-----------|
| Atomic executor places both legs within 500ms | Integration test with mock clients |
| Legging failure is detected and other leg cancelled | Test: one order succeeds, other times out → first is cancelled |
| Portfolio tracks positions and capital correctly | 10 trades executed and closed, realized PnL matches hand calc |
| Health monitor blocks trades when venue stale | Feed stops sending messages, trades are blocked after 5s |
| Risk engine enforces theme/event/daily limits | Attempt to exceed each limit, trade is rejected |
| Optimizer ranks by return/capital ratio | 5 opportunities with different ratios, order is correct |
| Full engine loop runs deterministically on replay | Same recording replayed twice produces identical trade log |

---

## 10. Known Limitations and Future Work

- **Legging tolerance:** Currently cancels if either leg fails. Could accept partial fills if slippage is small (future: add `max_slippage_cents` config).
- **Correlation hedging:** Risk engine checks themes independently. Could add correlation matrix to hedge black-swan scenarios (future phase 7).
- **Market impact:** Assumes filled at posted price. Real execution may move prices; could add impact model (future).
- **Settlement timing:** Assumes both venues settle at the same time. Real cross-venue trades settle asynchronously; needs to handle interim unhedged exposure (future phase 8).

---

## 11. Dependencies and Deliverables

**Input:** Phase 5 complete (registry, engine loop, dirty marking).

**Output:**
- `src/executor/mod.rs`, `atomic.rs`, `kalshi_client.rs`, `polymarket_client.rs`
- `src/portfolio/mod.rs`, `optimizer.rs`
- `src/health.rs`
- `src/risk/mod.rs`
- Updated `src/engine.rs` with integrated loop
- Updated `src/types.rs` with ExecutedTrade, Position, OrderStatus
- Tests: `tests/execution.rs`, `tests/portfolio.rs`, `tests/risk.rs`
- Integration test: full engine loop on replay with real trades

**Timeline:** 3–4 weeks with Claude Code.
