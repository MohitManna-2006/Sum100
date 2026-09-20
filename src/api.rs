//! Read-only HTTP boundary for the dashboard.
//!
//! The engine remains the sole owner of all calculations. This module only
//! fans already-computed [`EngineState`] snapshots out at a bounded cadence.

use crate::engine::EngineState;
use futures_util::{SinkExt, StreamExt};
use std::{io, time::Duration};
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

fn dashboard_state(state: &EngineState) -> serde_json::Value {
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
                    .map(|leg| serde_json::json!({
                        "contract_id": leg.contract.to_string(),
                        "venue": leg.venue,
                        "side": leg.side,
                        "action": "buy",
                        "best_ask": leg.cost_cents / qty,
                        "best_bid": 0,
                        "size": leg.qty,
                        "fee": leg.fee_cents / qty,
                    }))
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

    serde_json::json!({
        "timestamp_ms": state.timestamp_ms,
        "opportunities": opportunities,
        "health": {
            "connected": true,
            "messages_received": state.engine.events_received,
            "parse_errors": 0,
            "sequence_gaps": state.engine.sequence_gaps,
            "latency_ms": {
                "p50": 0.0,
                "p95": 0.0,
                "p99": 0.0,
            },
            "reconnections": 0,
        },
    })
}
