//! Read-only HTTP boundary for the dashboard.
//!
//! The engine remains the sole owner of all calculations. This module only
//! fans already-computed [`EngineState`] snapshots out at a bounded cadence.

use crate::engine::{ContractSummary, EngineState, SignalLeg};
use crate::types::Cents;
use futures_util::{SinkExt, StreamExt};
use std::{collections::HashMap, io, time::Duration};
use tokio::{net::TcpListener, sync::broadcast};
use tokio_tungstenite::{accept_async, tungstenite::Message};

pub async fn serve(
    listener: TcpListener,
    engine: broadcast::Sender<EngineState>,
) -> io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let states = engine.subscribe();
        tokio::spawn(async move {
            match accept_async(stream).await {
                Ok(socket) => {
                    tracing::info!(%peer, "dashboard websocket connected");
                    stream_engine(socket, states).await;
                }
                Err(error) => tracing::warn!(%peer, %error, "dashboard websocket rejected"),
            }
        });
    }
}

async fn stream_engine(
    socket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    mut states: broadcast::Receiver<EngineState>,
) {
    let (mut outgoing, mut incoming) = socket.split();
    let mut cadence = tokio::time::interval(Duration::from_millis(100));
    cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut latest: Option<EngineState> = None;

    loop {
        tokio::select! {
            message = incoming.next() => {
                match message {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            state = states.recv() => {
                match state {
                    Ok(state) => latest = Some(state),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = cadence.tick() => {
                let Some(state) = latest.take() else {
                    continue;
                };
                let Ok(json) = serde_json::to_string(&dashboard_state(&state)) else {
                    tracing::error!("failed to serialize engine state");
                    continue;
                };
                if outgoing.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Top of book for the outcome a leg buys, from the same snapshot the rest of
/// this frame is built from.
///
/// The ask is what the leg pays and the bid is what it could be sold back for,
/// so the pair is the spread the position was opened across. Neither is the
/// leg's blended fill price, which is already published as `cost_cents`: a
/// depth walk that consumed four levels has an average nobody was ever quoted.
fn top_of_book(leg: &SignalLeg, book: Option<&ContractSummary>) -> (Option<Cents>, Option<Cents>) {
    let Some(book) = book else {
        return (None, None);
    };
    match leg.side {
        "no" => (book.best_no_ask, book.best_no_bid),
        _ => (book.best_ask, book.best_bid),
    }
}

fn dashboard_state(state: &EngineState) -> serde_json::Value {
    let books: HashMap<u32, &ContractSummary> = state
        .contracts
        .iter()
        .map(|contract| (contract.contract, contract))
        .collect();

    let opportunities: Vec<_> = state
        .opportunities
        .iter()
        .map(|signal| {
            let qty = signal.qty.max(1);
            serde_json::json!({
                "id": format!("group-{}-{}", signal.group, signal.detected_at_ms),
                "group_id": format!("group-{}", signal.group),
                "relation": "constraint",
                "members": signal
                    .legs
                    .iter()
                    .map(|leg| leg.contract.to_string())
                    .collect::<Vec<_>>(),
                "legs": signal
                    .legs
                    .iter()
                    .map(|leg| {
                        let (ask, bid) = top_of_book(leg, books.get(&leg.contract).copied());
                        serde_json::json!({
                            "contract_id": leg.contract.to_string(),
                            "venue": leg.venue,
                            "side": leg.side,
                            "action": "buy",
                            // An empty side has no quote. The wire carries a
                            // number, so the fallback is the price this leg was
                            // actually costed at rather than a zero that would
                            // render as a free contract.
                            "best_ask": ask.unwrap_or(leg.cost_cents / qty),
                            "best_bid": bid.unwrap_or(0),
                            "size": leg.qty,
                            "fee": leg.fee_cents / qty,
                        })
                    })
                    .collect::<Vec<_>>(),
                "edge_cents": signal.net_cents / qty,
                "cost_cents": signal.capital_cents / qty,
                "fees_cents": signal.fees_cents / qty,
                "annualized_return": signal.annualized_return_percent / 100.0,
                "days_to_resolution": signal.days_to_resolution,
                "status": "accepted",
            })
        })
        .collect();

    // A feed that keeps no counters reports none, and the dashboard's own
    // contract is finite numbers, so those fields fall back to zero. The
    // `connected` flag beside them is the real reading either way, which is the
    // one that decides whether any of this is worth believing.
    let feed = state.health.feed.unwrap_or_default();
    // Frames off the socket, which is what the dashboard labels "messages". A
    // frame the parser rejected never became an engine event, so the two counts
    // differ, and the engine's is the fallback for a feed that keeps none.
    let messages_received = state
        .health
        .feed
        .map_or(state.engine.events_received, |feed| feed.messages_received);
    serde_json::json!({
        "timestamp_ms": state.timestamp_ms,
        "opportunities": opportunities,
        "health": {
            "connected": state.health.kalshi_healthy,
            "messages_received": messages_received,
            "parse_errors": feed.parse_errors,
            "sequence_gaps": state.engine.sequence_gaps,
            "latency_ms": {
                "p50": feed.latency_percentile_ms(0.50),
                "p95": feed.latency_percentile_ms(0.95),
                "p99": feed.latency_percentile_ms(0.99),
            },
            "reconnections": feed.reconnections,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{EngineMetrics, HealthStatus, Signal},
        health::HealthState,
        metrics::Metrics,
        portfolio::PortfolioSummary,
        risk::{RiskLimits, RiskStatus},
        solver::SolverMetrics,
        types::Venue,
    };

    /// A disconnected engine that has seen nothing, which is what the dashboard
    /// used to render as a healthy venue with no errors and no latency.
    fn silent_engine() -> EngineState {
        EngineState {
            timestamp_ms: 1_700_000_000_000,
            contracts: Vec::new(),
            opportunities: Vec::new(),
            engine: EngineMetrics::default(),
            solver: SolverMetrics::default(),
            portfolio: PortfolioSummary {
                capital_available_cents: 1_000_000,
                capital_locked_cents: 0,
                open_positions: 0,
                pnl_realized_cents: 0,
                pnl_unrealized_cents: 0,
                daily_loss_cents: 0,
                daily_loss_limit_cents: 100_000,
            },
            health: HealthStatus {
                kalshi_healthy: false,
                kalshi_state: HealthState::Disconnected,
                last_message_age_ms: 0,
                feed: None,
            },
            risk: RiskStatus {
                can_trade: false,
                reasons: Vec::new(),
                open_positions: 0,
                limits: RiskLimits::default(),
            },
        }
    }

    fn signal(legs: Vec<SignalLeg>) -> Signal {
        Signal {
            detected_at_ms: 1_700_000_000_000,
            group: 3,
            qty: 10,
            capital_cents: 950,
            fees_cents: 20,
            net_cents: 30,
            annualized_return_percent: 42.0,
            days_to_resolution: 1.0,
            legs,
            order_ids: Vec::new(),
            filled_cost_cents: 950,
        }
    }

    fn leg(contract: u32, side: &'static str) -> SignalLeg {
        SignalLeg {
            venue: Venue::Kalshi,
            contract,
            side,
            qty: 10,
            cost_cents: 450,
            fee_cents: 10,
        }
    }

    fn quoted_book(contract: u32) -> ContractSummary {
        ContractSummary {
            contract,
            venue: Venue::Kalshi,
            ticker: format!("KXBTCD-T{contract}"),
            state: "live",
            seq: 91,
            best_bid: Some(44),
            best_ask: Some(46),
            best_no_bid: Some(54),
            best_no_ask: Some(56),
            age_ms: 12,
        }
    }

    /// Every health field the dashboard renders must move when the engine's own
    /// counters move. The frame used to carry constants, so a dead feed and a
    /// perfect one serialized identically and the panel could not tell them
    /// apart; this pins both readings against the same projection.
    #[test]
    fn dashboard_state_reflects_actual_metrics() {
        let silent = dashboard_state(&silent_engine());
        let health = &silent["health"];
        assert_eq!(health["connected"], serde_json::json!(false));
        assert_eq!(health["parse_errors"], serde_json::json!(0));
        assert_eq!(health["reconnections"], serde_json::json!(0));
        assert_eq!(health["sequence_gaps"], serde_json::json!(0));
        assert_eq!(health["latency_ms"]["p50"], serde_json::json!(0.0));

        let mut feed = Metrics {
            messages_received: 4_812,
            parse_errors: 3,
            reconnections: 2,
            ..Metrics::default()
        };
        // Ten quick deltas and one that took nine seconds, so the median and the
        // tail have to disagree for the panel to be telling the truth.
        for _ in 0..10 {
            feed.observe_latency(8, 0);
        }
        feed.observe_latency(9_000, 0);

        let mut state = silent_engine();
        state.health.kalshi_healthy = true;
        state.health.kalshi_state = HealthState::Healthy;
        state.health.feed = Some(feed);
        state.engine.sequence_gaps = 5;
        state.engine.events_received = 4_700;

        let live = dashboard_state(&state);
        let health = &live["health"];
        assert_eq!(health["connected"], serde_json::json!(true));
        assert_eq!(health["parse_errors"], serde_json::json!(3));
        assert_eq!(health["reconnections"], serde_json::json!(2));
        assert_eq!(health["sequence_gaps"], serde_json::json!(5));
        // Frames off the socket, not events that survived the parser.
        assert_eq!(health["messages_received"], serde_json::json!(4_812));
        assert_eq!(health["latency_ms"]["p50"], serde_json::json!(10.0));
        assert_eq!(health["latency_ms"]["p99"], serde_json::json!(500.0));
    }

    /// Latency is only meaningful once something has been timed. Zero samples
    /// has to keep reporting zero rather than inheriting a bucket bound.
    #[test]
    fn an_unmeasured_feed_reports_zero_latency_rather_than_a_bucket_bound() {
        let mut state = silent_engine();
        state.health.feed = Some(Metrics {
            messages_received: 12,
            ..Metrics::default()
        });

        let latency = dashboard_state(&state)["health"]["latency_ms"].clone();
        assert_eq!(latency["p50"], serde_json::json!(0.0));
        assert_eq!(latency["p95"], serde_json::json!(0.0));
        assert_eq!(latency["p99"], serde_json::json!(0.0));
    }

    /// A leg that buys the no outcome is filled against resting yes bids, so it
    /// must be quoted off the no ladder. Publishing the yes quote for both sides
    /// would show a 46 cent ask on a leg that actually pays 56.
    #[test]
    fn a_leg_is_quoted_on_the_side_of_the_book_it_buys() {
        let mut state = silent_engine();
        state.contracts = vec![quoted_book(7)];
        state.opportunities = vec![signal(vec![leg(7, "yes"), leg(7, "no")])];

        let legs = dashboard_state(&state)["opportunities"][0]["legs"].clone();
        assert_eq!(legs[0]["best_ask"], serde_json::json!(46));
        assert_eq!(legs[0]["best_bid"], serde_json::json!(44));
        assert_eq!(legs[1]["best_ask"], serde_json::json!(56));
        assert_eq!(legs[1]["best_bid"], serde_json::json!(54));
    }

    /// An unquoted contract must not report a free one. The wire needs a number,
    /// and the honest one is the price the solver costed the leg at.
    #[test]
    fn a_leg_with_no_book_falls_back_to_the_costed_price() {
        let mut state = silent_engine();
        state.opportunities = vec![signal(vec![leg(7, "yes")])];

        let legs = dashboard_state(&state)["opportunities"][0]["legs"].clone();
        assert_eq!(legs[0]["best_ask"], serde_json::json!(45));
        assert_eq!(legs[0]["best_bid"], serde_json::json!(0));
    }
}
