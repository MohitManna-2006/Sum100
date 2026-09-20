//! Order placement.
//!
//! # This is the only module that can lose money
//!
//! Everything upstream is arithmetic on a copy of the venue's book. Being wrong
//! there costs a missed opportunity. Being wrong here costs cash, so the
//! defaults are chosen accordingly: [`paper::PaperOrderClient`] is what runs
//! unless a human passes an explicit flag, and the arbitrage legs go out
//! fill-or-kill so the venue itself refuses to leave a half-built position.
//!
//! # Why fill-or-kill
//!
//! An arbitrage is one position, not two trades. Buying one leg and missing the
//! other converts a risk-free trade into a naked directional bet at a price
//! nobody chose. The usual answer — "cancel the other order" — does not work:
//! a *filled* order cannot be cancelled, only unwound by trading back out
//! across the spread. Fill-or-kill pushes that problem to the venue, which can
//! actually enforce it atomically. When a leg does escape anyway,
//! [`atomic::ExecutionError::Legged`] says so in those words and hands back the
//! fills that need unwinding, rather than reporting a tidy cancellation that
//! did not happen.

pub mod atomic;
pub mod kalshi;
pub mod paper;

use crate::types::{Cents, ContractId, Side};
use serde::Serialize;
use std::{future::Future, pin::Pin};

/// Buying or selling an outcome.
///
/// Every leg the solver produces is a `Buy`: on these venues, selling the yes
/// outcome is the same trade as buying the no outcome, and phase 4 expresses
/// all four fast paths that way. `Sell` exists for unwinding a legged position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderAction {
    Buy,
    Sell,
}

/// How long an order may live before the venue kills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    /// All of it, immediately, or none of it. The only safe choice for a leg of
    /// a multi-leg position.
    FillOrKill,
    /// Take what is available now, kill the rest. Leaves a partial position.
    ImmediateOrCancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewOrder {
    pub contract_id: ContractId,
    /// The venue's own identifier, needed because the venue has never heard of
    /// a [`ContractId`].
    pub ticker: String,
    /// Which outcome is being traded.
    pub outcome: Side,
    pub action: OrderAction,
    pub quantity: i64,
    /// Worst acceptable price per contract. A leg that would fill worse than
    /// the price the solver costed is not the trade that was approved.
    pub limit_price: Cents,
    pub time_in_force: TimeInForce,
    /// Idempotency key, so a retry after a timeout cannot double-place.
    pub client_order_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderFill {
    pub order_id: String,
    pub contract_id: ContractId,
    pub outcome: Side,
    pub action: OrderAction,
    pub quantity_filled: i64,
    pub average_price: Cents,
    pub fee_cents: Cents,
    pub timestamp_ms: u64,
}

impl OrderFill {
    pub fn cost_cents(&self) -> Cents {
        self.quantity_filled * self.average_price + self.fee_cents
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum OrderError {
    /// The venue rejected the order outright.
    Rejected(String),
    /// Not enough resting size to fill the whole order, and the order was
    /// fill-or-kill, so nothing was done.
    Unfillable { wanted: i64, available: i64 },
    /// Network or transport failure. The order's fate is unknown.
    Transport(String),
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::Rejected(why) => write!(f, "venue rejected the order: {why}"),
            OrderError::Unfillable { wanted, available } => {
                write!(f, "wanted {wanted} contracts, {available} available")
            }
            OrderError::Transport(why) => {
                write!(f, "transport failure, order state unknown: {why}")
            }
        }
    }
}

impl std::error::Error for OrderError {}

/// Object-safe async boundary, matching [`crate::feed::Feed`]'s shape rather
/// than pulling in `async-trait` for one trait.
pub trait OrderClient: Send + Sync {
    fn place_order<'a>(
        &'a self,
        order: NewOrder,
    ) -> Pin<Box<dyn Future<Output = Result<OrderFill, OrderError>> + Send + 'a>>;

    fn cancel_order<'a>(
        &'a self,
        order_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), OrderError>> + Send + 'a>>;

    /// Whether this client can move real money. Used for logging and for the
    /// refusal in [`crate::engine`] when live trading was not opted into.
    fn is_live(&self) -> bool;

    /// Point a simulator at the current books. A live client ignores this: the
    /// venue is its own source of truth and a local book cannot override it.
    fn observe_books(&self, _books: &[crate::types::Book], _now_ms: u64) {}
}
