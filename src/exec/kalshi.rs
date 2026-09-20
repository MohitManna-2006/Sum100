//! Kalshi order placement. The only code in this repository that can spend money.
//!
//! Constructing this client is not enough to trade: [`crate::engine`] refuses a
//! live client unless the operator passed the explicit opt-in flag, and the
//! shipped configuration is paper mode. Both gates exist because the cost of
//! accidentally going live is unbounded and the cost of accidentally staying on
//! paper is a rerun.
//!
//! Every request is signed the same way the websocket handshake is: an RSA-PSS
//! signature over the concatenation of timestamp, method, and path.

use super::{NewOrder, OrderAction, OrderClient, OrderError, OrderFill, TimeInForce};
use crate::{
    clock::Clock,
    feed::kalshi::{Credentials, Environment, sign},
    types::{Cents, Side},
};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

const ORDERS_PATH: &str = "/trade-api/v2/portfolio/orders";

pub struct KalshiOrderClient {
    client: reqwest::Client,
    env: Environment,
    credentials: Credentials,
    clock: Arc<dyn Clock>,
}

#[derive(Debug, Serialize)]
struct OrderRequest<'a> {
    ticker: &'a str,
    client_order_id: &'a str,
    /// Kalshi's own vocabulary: `action` is buy or sell, `side` is yes or no.
    action: &'a str,
    side: &'a str,
    count: i64,
    #[serde(rename = "type")]
    order_type: &'a str,
    time_in_force: &'a str,
    /// Kalshi prices the side you are trading, in whole cents.
    #[serde(skip_serializing_if = "Option::is_none")]
    yes_price: Option<Cents>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_price: Option<Cents>,
}

#[derive(Debug, Deserialize)]
struct OrderResponse {
    order: OrderBody,
}

#[derive(Debug, Deserialize)]
struct OrderBody {
    order_id: String,
    /// Kept for the rejection log; a fill-or-kill that took nothing comes back
    /// as "canceled", which is the difference between a miss and an outage.
    #[serde(default)]
    #[allow(dead_code)]
    status: String,
    /// Contracts actually taken. Absent on a resting order, which for a
    /// fill-or-kill response means nothing was done.
    #[serde(default)]
    taker_fill_count: i64,
    /// Total premium in cents, as the venue computed it.
    #[serde(default)]
    taker_fill_cost: Cents,
    #[serde(default)]
    taker_fees: Cents,
}

impl KalshiOrderClient {
    /// Build a live order client. Requires credentials for `env`.
    pub fn new(env: Environment, clock: Arc<dyn Clock>) -> anyhow::Result<Self> {
        let credentials = Credentials::from_env()?;
        tracing::warn!(
            environment = env.name(),
            "LIVE ORDER CLIENT CONSTRUCTED; this client can place real orders"
        );
        Ok(KalshiOrderClient {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()?,
            env,
            credentials,
            clock,
        })
    }

    fn headers(&self, method: &str, path: &str) -> Result<reqwest::header::HeaderMap, OrderError> {
        let timestamp = i64::try_from(self.clock.now_ms())
            .map_err(|e| OrderError::Transport(format!("clock out of range: {e}")))?;
        let signature = sign(&self.credentials.key, timestamp, method, path)
            .map_err(|e| OrderError::Transport(format!("signing failed: {e}")))?;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("KALSHI-ACCESS-KEY", self.credentials.key_id.clone());
        let mut insert = |name: &'static str, value: &str| -> Result<(), OrderError> {
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| OrderError::Transport(format!("bad header {name}: {e}")))?;
            headers.insert(name, value);
            Ok(())
        };
        insert("KALSHI-ACCESS-TIMESTAMP", &timestamp.to_string())?;
        insert("KALSHI-ACCESS-SIGNATURE", &signature)?;
        Ok(headers)
    }
}

/// Translate an engine order into Kalshi's request vocabulary.
///
/// Kalshi prices whichever side you name, so a yes order carries `yes_price`
/// and a no order carries `no_price`. Sending both, or the wrong one, is a
/// silently different trade.
fn to_request<'a>(order: &'a NewOrder, tif: &'a str) -> OrderRequest<'a> {
    let (yes_price, no_price) = match order.outcome {
        Side::Yes => (Some(order.limit_price), None),
        Side::No => (None, Some(order.limit_price)),
    };
    OrderRequest {
        ticker: &order.ticker,
        client_order_id: &order.client_order_id,
        action: match order.action {
            OrderAction::Buy => "buy",
            OrderAction::Sell => "sell",
        },
        side: match order.outcome {
            Side::Yes => "yes",
            Side::No => "no",
        },
        count: order.quantity,
        order_type: "limit",
        time_in_force: tif,
        yes_price,
        no_price,
    }
}

fn tif_str(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::FillOrKill => "fill_or_kill",
        TimeInForce::ImmediateOrCancel => "immediate_or_cancel",
    }
}

impl OrderClient for KalshiOrderClient {
    fn place_order<'a>(
        &'a self,
        order: NewOrder,
    ) -> Pin<Box<dyn Future<Output = Result<OrderFill, OrderError>> + Send + 'a>> {
        Box::pin(async move {
            let headers = self.headers("POST", ORDERS_PATH)?;
            let body = to_request(&order, tif_str(order.time_in_force));
            let response = self
                .client
                .post(format!("{}{ORDERS_PATH}", self.env.rest_url()))
                .headers(headers)
                .json(&body)
                .send()
                .await
                .map_err(|e| OrderError::Transport(e.to_string()))?;

            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                // A rejection is a clean no-op; a 5xx is not, because the order
                // may have been accepted before the failure.
                return Err(if status.is_client_error() {
                    OrderError::Rejected(format!("{status}: {detail}"))
                } else {
                    OrderError::Transport(format!("{status}: {detail}"))
                });
            }

            let parsed: OrderResponse = response
                .json()
                .await
                .map_err(|e| OrderError::Transport(format!("unreadable order response: {e}")))?;
            let filled = parsed.order.taker_fill_count;
            if filled < order.quantity {
                // Fill-or-kill means the venue did nothing. Reporting a partial
                // fill it did not make would corrupt the position record.
                return Err(OrderError::Unfillable {
                    wanted: order.quantity,
                    available: filled,
                });
            }
            Ok(OrderFill {
                order_id: parsed.order.order_id,
                contract_id: order.contract_id,
                outcome: order.outcome,
                action: order.action,
                quantity_filled: filled,
                // The venue's own cost is authoritative over any local estimate.
                average_price: parsed.order.taker_fill_cost / filled.max(1),
                fee_cents: parsed.order.taker_fees,
                timestamp_ms: self.clock.now_ms(),
            })
        })
    }

    fn cancel_order<'a>(
        &'a self,
        order_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), OrderError>> + Send + 'a>> {
        Box::pin(async move {
            let path = format!("{ORDERS_PATH}/{order_id}");
            let headers = self.headers("DELETE", &path)?;
            let response = self
                .client
                .delete(format!("{}{path}", self.env.rest_url()))
                .headers(headers)
                .send()
                .await
                .map_err(|e| OrderError::Transport(e.to_string()))?;
            if response.status().is_success() {
                return Ok(());
            }
            Err(OrderError::Rejected(format!(
                "cancel {order_id}: {}",
                response.status()
            )))
        })
    }

    fn is_live(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContractId;

    fn order(outcome: Side) -> NewOrder {
        NewOrder {
            contract_id: ContractId(0),
            ticker: "KXFED-26SEP-C25".into(),
            outcome,
            action: OrderAction::Buy,
            quantity: 100,
            limit_price: 45,
            time_in_force: TimeInForce::FillOrKill,
            client_order_id: "sum100-1".into(),
        }
    }

    /// The request body is the contract with the venue; a wrong field here is a
    /// different trade placed with real money.
    #[test]
    fn the_request_body_names_the_side_being_priced() {
        let yes = serde_json::to_value(to_request(&order(Side::Yes), "fill_or_kill")).unwrap();
        assert_eq!(yes["side"], "yes");
        assert_eq!(yes["action"], "buy");
        assert_eq!(yes["count"], 100);
        assert_eq!(yes["type"], "limit");
        assert_eq!(yes["time_in_force"], "fill_or_kill");
        assert_eq!(yes["yes_price"], 45);
        assert!(yes.get("no_price").is_none(), "only one side is priced");

        let no = serde_json::to_value(to_request(&order(Side::No), "fill_or_kill")).unwrap();
        assert_eq!(no["side"], "no");
        assert_eq!(no["no_price"], 45);
        assert!(no.get("yes_price").is_none());
    }

    #[test]
    fn a_fill_or_kill_response_with_nothing_taken_is_not_a_fill() {
        let body: OrderResponse = serde_json::from_str(
            r#"{"order":{"order_id":"abc","status":"canceled","taker_fill_count":0}}"#,
        )
        .unwrap();
        assert_eq!(body.order.taker_fill_count, 0);
        assert_eq!(body.order.status, "canceled");
        assert_eq!(body.order.taker_fees, 0);
    }
}
