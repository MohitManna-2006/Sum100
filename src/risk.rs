//! Position limits: the answer to "this trade is profitable, but should we?"
//!
//! The solver proves a trade cannot lose at settlement. That is not the same as
//! it being safe to put on. A guaranteed dollar is only guaranteed if the
//! contracts resolve the way the registry says they do, and the registry is a
//! human-maintained file. Concentration is therefore the real exposure: not to
//! price, but to one resolution rule being written down wrong.
//!
//! Limits are checked against capital *locked*, not against expected profit,
//! because the amount at stake if a group turns out to be mis-specified is the
//! whole position, not its edge.

use crate::{
    portfolio::Portfolio,
    registry::{EventId, Registry},
    types::Cents,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RiskLimits {
    /// Most capital that may be locked in any one event.
    pub max_per_event_cents: Cents,
    /// Most capital across every event sharing a theme, such as all Fed
    /// decisions. Separate events resolving off one source fail together.
    pub max_per_theme_cents: Cents,
    pub max_concurrent_trades: usize,
}

impl Default for RiskLimits {
    fn default() -> Self {
        RiskLimits {
            max_per_event_cents: 50_000,
            max_per_theme_cents: 100_000,
            max_concurrent_trades: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskError {
    EventLimitExceeded { would_be: Cents, limit: Cents },
    ThemeLimitExceeded { would_be: Cents, limit: Cents },
    ConcurrentTradeLimit { open: usize, limit: usize },
}

impl RiskError {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskError::EventLimitExceeded { .. } => "event_limit",
            RiskError::ThemeLimitExceeded { .. } => "theme_limit",
            RiskError::ConcurrentTradeLimit { .. } => "concurrent_trade_limit",
        }
    }
}

impl std::fmt::Display for RiskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskError::EventLimitExceeded { would_be, limit } => {
                write!(f, "event exposure would be {would_be} cents, limit {limit}")
            }
            RiskError::ThemeLimitExceeded { would_be, limit } => {
                write!(f, "theme exposure would be {would_be} cents, limit {limit}")
            }
            RiskError::ConcurrentTradeLimit { open, limit } => {
                write!(f, "{open} open positions, limit {limit}")
            }
        }
    }
}

impl std::error::Error for RiskError {}

/// Decide whether one more position fits inside the limits.
///
/// Every check is "what would exposure become", not "what is it now", so a
/// single trade cannot step over a limit it was under before placing.
pub fn check(
    portfolio: &Portfolio,
    registry: &Registry,
    event: EventId,
    theme: Option<&str>,
    trade_cost_cents: Cents,
    limits: &RiskLimits,
) -> Result<(), RiskError> {
    let open = portfolio.open_count();
    if open >= limits.max_concurrent_trades {
        return Err(RiskError::ConcurrentTradeLimit {
            open,
            limit: limits.max_concurrent_trades,
        });
    }

    let event_exposure = portfolio.exposure_to(event) + trade_cost_cents;
    if event_exposure > limits.max_per_event_cents {
        return Err(RiskError::EventLimitExceeded {
            would_be: event_exposure,
            limit: limits.max_per_event_cents,
        });
    }

    // An event with no theme is only limited per event. Inventing a theme for
    // it would silently pool unrelated exposure under one budget.
    if let Some(theme) = theme {
        let mut theme_exposure = trade_cost_cents;
        for position in portfolio.open_positions() {
            let same_theme = registry
                .event(position.event_id)
                .and_then(|e| e.theme.as_deref())
                .is_some_and(|held| held == theme);
            if same_theme {
                theme_exposure += position.cost_cents;
            }
        }
        if theme_exposure > limits.max_per_theme_cents {
            return Err(RiskError::ThemeLimitExceeded {
                would_be: theme_exposure,
                limit: limits.max_per_theme_cents,
            });
        }
    }

    Ok(())
}

/// What the API layer publishes about risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RiskStatus {
    pub can_trade: bool,
    /// Why not, when `can_trade` is false. Empty means nothing is blocking.
    pub reasons: Vec<String>,
    pub open_positions: usize,
    pub limits: RiskLimits,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        portfolio::{Position, PositionLeg},
        registry::Relation,
        types::{ContractId, Side},
    };

    const NOW: u64 = 1_789_343_120_404;

    fn registry_with_themes() -> Registry {
        Registry::parse(
            r#"
[[event]]
id = "fed-sep"
description = "FOMC September"
resolves_at = "2026-09-17T18:00:00Z"
resolution_source = "FOMC statement"
theme = "fed"

[[event.group]]
type = "complement"
members = [{ venue = "kalshi", ticker = "KXFED-26SEP-C25" }]

[[event]]
id = "fed-oct"
description = "FOMC October"
resolves_at = "2026-10-29T18:00:00Z"
resolution_source = "FOMC statement"
theme = "fed"

[[event.group]]
type = "complement"
members = [{ venue = "kalshi", ticker = "KXFED-26OCT-C25" }]

[[event]]
id = "btc"
description = "BTC hourly"
resolves_at = "2026-09-14T21:00:00Z"
resolution_source = "Kalshi"

[[event.group]]
type = "complement"
members = [{ venue = "kalshi", ticker = "KXBTCD-26SEP1417-T70000" }]
"#,
        )
        .unwrap()
    }

    fn position(event: EventId, cost_cents: Cents) -> Position {
        Position {
            event_id: event,
            group: 0,
            legs: vec![PositionLeg {
                contract_id: ContractId(0),
                side: Side::Yes,
                quantity: 1,
                average_entry_price: cost_cents,
                order_ids: Vec::new(),
            }],
            cost_cents,
            fees_cents: 0,
            entry_time_ms: NOW,
            resolved: false,
            pnl_realized_cents: 0,
            pnl_unrealized_cents: 0,
        }
    }

    fn portfolio_holding(positions: &[(EventId, Cents)]) -> Portfolio {
        let mut portfolio = Portfolio::new(10_000_000, 1_000_000, NOW);
        for (event, cost) in positions {
            portfolio.add_position(position(*event, *cost)).unwrap();
        }
        portfolio
    }

    #[test]
    fn a_trade_cannot_step_over_the_event_limit_it_was_under() {
        let registry = registry_with_themes();
        let limits = RiskLimits::default();
        let portfolio = portfolio_holding(&[(EventId(0), 45_000)]);

        // 45000 held plus 5000 is exactly the 50000 limit: allowed.
        assert!(check(&portfolio, &registry, EventId(0), None, 5_000, &limits).is_ok());
        // One cent more is not.
        assert_eq!(
            check(&portfolio, &registry, EventId(0), None, 5_001, &limits),
            Err(RiskError::EventLimitExceeded {
                would_be: 50_001,
                limit: 50_000,
            })
        );
        // A different event has its own budget.
        assert!(check(&portfolio, &registry, EventId(2), None, 40_000, &limits).is_ok());
    }

    #[test]
    fn theme_exposure_pools_across_separate_events() {
        let registry = registry_with_themes();
        let limits = RiskLimits::default();
        // Two different Fed events, 45000 each, both under the event limit.
        let portfolio = portfolio_holding(&[(EventId(0), 45_000), (EventId(1), 45_000)]);
        assert!(check(&portfolio, &registry, EventId(0), None, 5_000, &limits).is_ok());

        // Together they are 90000 of one resolution source; 10001 more breaks
        // the theme budget even though the event budget has room.
        assert_eq!(
            check(
                &portfolio,
                &registry,
                EventId(0),
                Some("fed"),
                5_000,
                &limits
            ),
            Ok(())
        );
        assert_eq!(
            check(
                &portfolio,
                &registry,
                EventId(2),
                Some("fed"),
                10_001,
                &limits
            ),
            Err(RiskError::ThemeLimitExceeded {
                would_be: 100_001,
                limit: 100_000,
            })
        );
        // The untethered BTC event does not draw on the Fed budget.
        assert!(check(&portfolio, &registry, EventId(2), None, 40_000, &limits).is_ok());
    }

    #[test]
    fn the_concurrency_limit_counts_only_open_positions() {
        let registry = registry_with_themes();
        let limits = RiskLimits {
            max_concurrent_trades: 2,
            ..RiskLimits::default()
        };
        let mut portfolio = portfolio_holding(&[(EventId(0), 100), (EventId(1), 100)]);
        assert_eq!(
            check(&portfolio, &registry, EventId(2), None, 100, &limits),
            Err(RiskError::ConcurrentTradeLimit { open: 2, limit: 2 })
        );
        // Settling one frees a slot.
        portfolio.realize(EventId(0), true).unwrap();
        assert!(check(&portfolio, &registry, EventId(2), None, 100, &limits).is_ok());
    }

    #[test]
    fn an_empty_portfolio_accepts_a_trade_inside_every_limit() {
        let registry = registry_with_themes();
        let portfolio = portfolio_holding(&[]);
        assert!(
            check(
                &portfolio,
                &registry,
                EventId(0),
                Some("fed"),
                10_000,
                &RiskLimits::default()
            )
            .is_ok()
        );
        // Relations are untouched by risk; this only guards the registry shape
        // the themes were read from.
        assert!(matches!(
            registry
                .group(crate::registry::GroupId(0))
                .unwrap()
                .relation,
            Relation::Complement { .. }
        ));
    }
}
