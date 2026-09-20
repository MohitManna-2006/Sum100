use super::{Feed, FeedEvent};
use crate::{
    clock::Clock,
    metrics::Metrics,
    record::{Control, Recorder},
    types::{Contracts, Level, Side, Venue, parse_price_cents, parse_size_contracts},
};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rsa::{
    RsaPrivateKey,
    pkcs1::DecodeRsaPrivateKey,
    pkcs8::DecodePrivateKey,
    pss::SigningKey,
    signature::{RandomizedSigner, SignatureEncoding},
};
use serde::Deserialize;
use sha2::Sha256;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

pub const WS_PATH: &str = "/trade-api/ws/v2";
#[derive(Debug, Clone, Copy, Default)]
pub enum Environment {
    #[default]
    Demo,
    Production,
}
impl Environment {
    pub fn ws_url(self) -> &'static str {
        match self {
            Self::Demo => "wss://external-api-ws.demo.kalshi.co/trade-api/ws/v2",
            Self::Production => "wss://external-api-ws.kalshi.com/trade-api/ws/v2",
        }
    }
    pub fn rest_url(self) -> &'static str {
        match self {
            Self::Demo => "https://external-api.demo.kalshi.co/trade-api/v2",
            Self::Production => "https://external-api.kalshi.com/trade-api/v2",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Demo => "demo",
            Self::Production => "production",
        }
    }
}

pub struct Credentials {
    /// Readable so the order client can sign its own REST requests exactly the
    /// way the websocket handshake signs its own, from one loaded key.
    pub key_id: HeaderValue,
    pub key: RsaPrivateKey,
}
impl Credentials {
    pub fn from_env() -> Result<Self> {
        let key_id = std::env::var("KALSHI_KEY_ID").context("KALSHI_KEY_ID must be set")?;
        ensure!(!key_id.is_empty(), "KALSHI_KEY_ID is empty");
        let path = std::env::var("KALSHI_PRIVATE_KEY_PATH")
            .context("KALSHI_PRIVATE_KEY_PATH must be set")?;
        let pem = std::fs::read_to_string(path).context("reading private key")?;
        let key = RsaPrivateKey::from_pkcs1_pem(&pem)
            .or_else(|_| RsaPrivateKey::from_pkcs8_pem(&pem))
            .context("key must be PKCS#1 or PKCS#8 RSA PEM")?;
        key.validate().context("invalid RSA key")?;
        // Exercise signing at startup, including key length and OS entropy errors.
        sign(&key, 0, "GET", WS_PATH)?;
        Ok(Self {
            key_id: HeaderValue::from_str(&key_id)?,
            key,
        })
    }
}
pub fn sign(key: &RsaPrivateKey, ts: i64, method: &str, path: &str) -> Result<String> {
    let path = path.split('?').next().context("missing signing path")?;
    let message = format!("{ts}{method}{path}");
    let sig = SigningKey::<Sha256>::new(key.clone())
        .try_sign_with_rng(&mut rsa::rand_core::OsRng, message.as_bytes())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()))
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub async fn connect(
    env: Environment,
    credentials: &Credentials,
    tickers: &[String],
    channels: &[&str],
    clock: &dyn Clock,
) -> Result<Socket> {
    // The venue authenticates against real time; callers pass a wall clock.
    let timestamp = i64::try_from(clock.now_ms())?;
    let mut request = env.ws_url().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("KALSHI-ACCESS-KEY", credentials.key_id.clone());
    headers.insert(
        "KALSHI-ACCESS-TIMESTAMP",
        HeaderValue::from_str(&timestamp.to_string())?,
    );
    headers.insert(
        "KALSHI-ACCESS-SIGNATURE",
        HeaderValue::from_str(&sign(&credentials.key, timestamp, "GET", WS_PATH)?)?,
    );
    let connect_result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(request),
    )
    .await;
    let (mut socket, _) = connect_result??;
    socket.send(Message::Text(serde_json::json!({"id":1,"cmd":"subscribe","params":{"channels":channels,"market_tickers":tickers}}).to_string().into())).await?;
    tracing::info!(
        url = env.ws_url(),
        ?tickers,
        "connected and subscription sent"
    );
    Ok(socket)
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    seq: Option<u64>,
    #[serde(default)]
    msg: serde_json::Value,
}
#[derive(Deserialize)]
struct Snapshot {
    market_ticker: String,
    #[serde(default)]
    yes_dollars_fp: SnapshotSide,
    #[serde(default)]
    no_dollars_fp: SnapshotSide,
    ts_ms: Option<u64>,
}

/// How one side of a snapshot arrived. Kalshi omits the key of a side with no
/// resting orders (observed for far strikes), so absence is a valid empty
/// side, distinct from a malformed one: any value other than null or a list
/// of `[price, size]` string pairs fails deserialization.
#[derive(Default)]
enum SnapshotSide {
    #[default]
    Absent,
    Null,
    Levels(Vec<(String, String)>),
}

impl<'de> Deserialize<'de> for SnapshotSide {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(
            match Option::<Vec<(String, String)>>::deserialize(deserializer)? {
                None => Self::Null,
                Some(rows) => Self::Levels(rows),
            },
        )
    }
}
#[derive(Deserialize)]
struct Delta {
    market_ticker: String,
    price_dollars: String,
    delta_fp: String,
    side: String,
    ts_ms: Option<u64>,
    ts: Option<String>,
}

pub struct Parser {
    pub contracts: Contracts,
    pub metrics: Metrics,
}
impl Parser {
    pub fn new(tickers: &[String]) -> Result<Self> {
        let mut contracts = Contracts::default();
        for ticker in tickers {
            contracts.intern(Venue::Kalshi, ticker)?;
        }
        Ok(Self {
            contracts,
            metrics: Metrics::default(),
        })
    }
    pub fn parse(&mut self, raw: &str, receipt: u64) -> Option<FeedEvent> {
        self.metrics.parse_attempts += 1;
        match self.parse_inner(raw, receipt) {
            Ok(event) => event,
            Err(error) => {
                self.metrics.parse_errors += 1;
                tracing::warn!(%error, raw, "venue payload rejected");
                None
            }
        }
    }
    fn parse_inner(&mut self, raw: &str, receipt: u64) -> Result<Option<FeedEvent>> {
        let e: Envelope = serde_json::from_str(raw)?;
        let mut discarded = 0;
        let mut absent_sides = 0;
        let event = match e.kind.as_str() {
            "orderbook_snapshot" => {
                let m: Snapshot = serde_json::from_value(e.msg)?;
                absent_sides = [&m.yes_dollars_fp, &m.no_dollars_fp]
                    .into_iter()
                    .filter(|side| matches!(side, SnapshotSide::Absent))
                    .count() as u64;
                // One present side proves the payload uses this schema. With
                // both keys absent, an empty market and renamed keys look
                // identical, so that stays a schema error.
                ensure!(absent_sides < 2, "snapshot has neither side");
                let contract = self
                    .contracts
                    .get(Venue::Kalshi, &m.market_ticker)
                    .context("snapshot for unsubscribed ticker")?;
                let mut levels = |side: SnapshotSide| -> Result<Vec<Level>> {
                    let rows = match side {
                        SnapshotSide::Absent | SnapshotSide::Null => Vec::new(),
                        SnapshotSide::Levels(rows) => rows,
                    };
                    rows.into_iter()
                        .map(|(p, s)| {
                            ensure!(!s.starts_with('-'), "negative snapshot size");
                            Ok(Level {
                                price: parse_price_cents(&p)?,
                                size: parse_size_contracts(&s, &mut discarded)?,
                            })
                        })
                        .collect()
                };
                FeedEvent::Snapshot {
                    contract,
                    yes: levels(m.yes_dollars_fp)?,
                    no: levels(m.no_dollars_fp)?,
                    seq: e.seq.context("snapshot missing seq")?,
                    venue_ts_ms: m.ts_ms,
                }
            }
            "orderbook_delta" => {
                let m: Delta = serde_json::from_value(e.msg)?;
                let venue_ts_ms = match m.ts_ms {
                    Some(ts) => ts,
                    None => u64::try_from(
                        chrono::DateTime::parse_from_rfc3339(
                            m.ts.as_deref().context("delta missing timestamp")?,
                        )?
                        .timestamp_millis(),
                    )?,
                };
                FeedEvent::Delta {
                    contract: self
                        .contracts
                        .get(Venue::Kalshi, &m.market_ticker)
                        .context("delta for unsubscribed ticker")?,
                    side: match m.side.as_str() {
                        "yes" => Side::Yes,
                        "no" => Side::No,
                        _ => bail!("unknown orderbook side"),
                    },
                    price: parse_price_cents(&m.price_dollars)?,
                    size_delta: parse_size_contracts(&m.delta_fp, &mut discarded)?,
                    seq: e.seq.context("delta missing seq")?,
                    venue_ts_ms,
                }
            }
            "subscribed" | "unsubscribed" => return Ok(None),
            "error" => bail!("venue error: {}", e.msg),
            _ => {
                self.metrics.unknown_messages += 1;
                tracing::debug!(kind = e.kind, "skipping unknown message type");
                return Ok(None);
            }
        };
        self.metrics.discarded_size_hundredths = self
            .metrics
            .discarded_size_hundredths
            .saturating_add(discarded);
        self.metrics.snapshot_sides_absent += absent_sides;
        if let FeedEvent::Delta { venue_ts_ms, .. } = event {
            self.metrics.observe_latency(receipt, venue_ts_ms);
        }
        Ok(Some(event))
    }
}

pub fn backoff_ms(attempt: u32, jitter: u64) -> u64 {
    let base = 250u64.saturating_mul(1u64 << attempt.min(7)).min(30_000);
    (base + jitter % (base / 4 + 1)).min(30_000)
}

pub struct KalshiFeed {
    events: mpsc::Receiver<FeedEvent>,
    shutdown: watch::Sender<bool>,
    resync: watch::Sender<u64>,
    /// Latest counters published by the worker, which owns the parser.
    metrics: watch::Receiver<Metrics>,
    worker: JoinHandle<Result<Metrics>>,
}
impl Feed for KalshiFeed {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>> {
        Box::pin(self.events.recv())
    }

    fn request_resync(&self) {
        KalshiFeed::request_resync(self);
    }

    /// A copy of what the worker last published.
    ///
    /// The parser lives inside the spawned task, so this cannot be a borrow. A
    /// [`watch`] channel keeps the cost on the worker to one store per frame and
    /// leaves the read side lock-free enough to sit on the engine's publish
    /// path, which is the only place it is read.
    fn metrics(&self) -> Option<Metrics> {
        Some(*self.metrics.borrow())
    }
}
impl KalshiFeed {
    /// `clock` supplies receipt timestamps and signing time; live callers pass
    /// a [`crate::clock::WallClock`].
    pub fn start(
        env: Environment,
        tickers: Vec<String>,
        mut recorder: Recorder,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        ensure!(
            !tickers.is_empty() && tickers.iter().all(|s| !s.trim().is_empty()),
            "at least one nonempty ticker is required"
        );
        let credentials = Credentials::from_env()?;
        let mut parser = Parser::new(&tickers)?;
        parser.metrics.bytes_recorded += recorder.write_control(
            clock.now_ms(),
            &Control::SessionStarted {
                environment: env.name().into(),
                tickers: tickers.clone(),
            },
        )?;
        let (send, events) = mpsc::channel(256);
        let (shutdown, stop) = watch::channel(false);
        let (resync_tx, resync_rx) = watch::channel(0u64);
        let (metrics_tx, metrics_rx) = watch::channel(parser.metrics);
        let link = Link {
            env,
            credentials,
            tickers,
            clock,
        };
        let worker = tokio::spawn(async move {
            let mut recorder = recorder;
            let mut parser = parser;
            let result = run(
                &link,
                &mut recorder,
                &mut parser,
                send,
                resync_rx,
                stop,
                &metrics_tx,
            )
            .await;
            recorder.flush()?;
            parser.metrics.log(Venue::Kalshi);
            // A final publish so a dashboard reading after the socket died sees
            // the counts that explain why, not the ones from the last good frame.
            metrics_tx.send_replace(parser.metrics);
            result?;
            Ok(parser.metrics)
        });
        Ok(Self {
            events,
            shutdown,
            resync: resync_tx,
            metrics: metrics_rx,
            worker,
        })
    }

    /// Force a socket drop so the feed reconnects and receives a fresh snapshot.
    pub fn request_resync(&self) {
        let next = self.resync.borrow().saturating_add(1);
        let _ = self.resync.send(next);
    }

    /// Ask the worker to stop at the next frame boundary. Keep calling `next`
    /// until it returns `None` to receive every event whose bytes were
    /// recorded, so the consumer's final state matches the recording.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Stop, discard undelivered events, and join the worker.
    pub async fn shutdown(mut self) -> Result<Metrics> {
        self.stop();
        // Draining also unblocks a worker waiting on a full channel.
        while self.events.recv().await.is_some() {}
        self.worker.await?
    }
}

struct Link {
    env: Environment,
    credentials: Credentials,
    tickers: Vec<String>,
    clock: Arc<dyn Clock>,
}

/// Stop is only observed between frames: once a frame or control envelope is
/// recorded, its events are always sent, so replay never sees events the live
/// consumer did not.
async fn run(
    link: &Link,
    recorder: &mut Recorder,
    parser: &mut Parser,
    send: mpsc::Sender<FeedEvent>,
    mut resync: watch::Receiver<u64>,
    mut stop: watch::Receiver<bool>,
    metrics: &watch::Sender<Metrics>,
) -> Result<()> {
    let Link {
        env,
        ref credentials,
        ref tickers,
        ref clock,
    } = *link;
    let mut flush = tokio::time::interval(Duration::from_secs(2));
    let mut summary = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    let mut attempt = 0u32;
    let mut reconnect = false;
    loop {
        // Timers remain active during backoff and slow handshakes as well.
        let establish = async move {
            if reconnect {
                tokio::time::sleep(Duration::from_millis(backoff_ms(attempt, rand::random())))
                    .await;
            }
            connect(
                env,
                credentials,
                tickers,
                &["orderbook_delta"],
                clock.as_ref(),
            )
            .await
        };
        tokio::pin!(establish);
        let connection = loop {
            tokio::select! {
                result = &mut establish => break result,
                _ = flush.tick() => recorder.flush()?,
                _ = summary.tick() => parser.metrics.log(Venue::Kalshi),
                _ = stop.changed() => return Ok(()),
            }
        };
        let mut socket = match connection {
            Ok(socket) => {
                if reconnect {
                    parser.metrics.bytes_recorded += recorder
                        .write_control(clock.now_ms(), &Control::Reconnected { attempt })?;
                }
                socket
            }
            Err(error) => {
                tracing::warn!(%error, "connection failed; retrying");
                if !reconnect {
                    parser.metrics.bytes_recorded += recorder.write_control(
                        clock.now_ms(),
                        &Control::Disconnected {
                            reason: format!("connection failed: {error}"),
                        },
                    )?;
                    send.send(FeedEvent::Disconnected {
                        venue: Venue::Kalshi,
                    })
                    .await?;
                }
                if reconnect {
                    attempt = attempt.saturating_add(1);
                }
                reconnect = true;
                continue;
            }
        };
        let mut acknowledged = false;
        let subscription_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut last_frame = tokio::time::Instant::now();
        // Clear any pending resync notification from before this connection.
        while resync.has_changed().unwrap_or(false) {
            resync.borrow_and_update();
        }
        let reason = loop {
            let frame = tokio::select! {
                frame = socket.next() => frame,
                _ = flush.tick() => { recorder.flush()?; continue; }
                _ = summary.tick() => { parser.metrics.log(Venue::Kalshi); continue; }
                _ = stop.changed() => return Ok(()),
                _ = resync.changed() => {
                    tracing::warn!("resync requested; dropping socket");
                    break "resync requested".to_owned();
                }
                _ = tokio::time::sleep_until(subscription_deadline), if !acknowledged => {
                    tracing::warn!("subscription acknowledgement timed out");
                    break "subscription acknowledgement timed out".to_owned();
                }
                _ = tokio::time::sleep_until(last_frame + Duration::from_secs(45)) => {
                    tracing::warn!("websocket idle timeout");
                    break "websocket idle timeout".to_owned();
                }
            };
            let frame = match frame {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => {
                    tracing::warn!(%error, "websocket disconnected");
                    break format!("websocket error: {error}");
                }
                None => break "websocket stream ended".to_owned(),
            };
            // Monotonic instant only schedules the idle timeout; it never
            // reaches an event, a book, or a recording.
            last_frame = tokio::time::Instant::now();
            let received = clock.now_ms();
            parser.metrics.messages_received += 1;
            let (kind, raw) = match &frame {
                Message::Text(text) => ("text", text.to_string()),
                Message::Binary(bytes) => (
                    "binary_base64",
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                ),
                Message::Ping(bytes) => (
                    "ping_base64",
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                ),
                Message::Pong(bytes) => (
                    "pong_base64",
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                ),
                Message::Close(close) => {
                    let bytes = close
                        .as_ref()
                        .map(|c| {
                            let mut b = u16::from(c.code).to_be_bytes().to_vec();
                            b.extend_from_slice(c.reason.as_bytes());
                            b
                        })
                        .unwrap_or_default();
                    (
                        "close_base64",
                        base64::engine::general_purpose::STANDARD.encode(bytes),
                    )
                }
                Message::Frame(_) => continue, // tungstenite never yields raw frames.
            };
            // This must precede every parser, including subscription handling.
            parser.metrics.bytes_recorded += recorder.write(received, kind, &raw)?;
            if let Message::Text(_) = frame {
                let control: Option<serde_json::Value> = serde_json::from_str(&raw).ok();
                if let Some(ref value) = control
                    && value["type"] == "subscribed"
                    && value["id"] == 1
                    && value["msg"]["channel"] == "orderbook_delta"
                    && !acknowledged
                {
                    acknowledged = true;
                    attempt = 0;
                    if reconnect {
                        parser.metrics.reconnections += 1;
                        parser.metrics.bytes_recorded += recorder.write_control(
                            received,
                            &Control::Resubscribed {
                                tickers: tickers.clone(),
                            },
                        )?;
                        for ticker in tickers {
                            let contract = parser
                                .contracts
                                .get(Venue::Kalshi, ticker)
                                .context("missing interned ticker")?;
                            send.send(FeedEvent::Resubscribed { contract }).await?;
                        }
                    }
                }
                if let Some(event) = parser.parse(&raw, received) {
                    send.send(event).await?;
                }
                if control.as_ref().is_some_and(|v| v["type"] == "error") {
                    break "venue error message".to_owned();
                }
            } else if matches!(frame, Message::Close(_)) {
                break "venue close frame".to_owned();
            }
            // One store per frame, next to a JSON parse and a gzip write that
            // each cost far more. The frames that break out of this loop instead
            // are covered by the publish after the disconnect envelope below.
            metrics.send_replace(parser.metrics);
            // Flush automatic pong replies even if the next inbound frame stalls.
            match tokio::time::timeout(Duration::from_secs(5), socket.flush()).await {
                Ok(Ok(())) => {}
                result => {
                    tracing::warn!(?result, "websocket flush failed or timed out");
                    break "websocket flush failed or timed out".to_owned();
                }
            }
        };
        parser.metrics.bytes_recorded += recorder.write_control(
            clock.now_ms(),
            &Control::Disconnected {
                reason: reason.clone(),
            },
        )?;
        metrics.send_replace(parser.metrics);
        tracing::warn!(reason, "feed disconnected");
        send.send(FeedEvent::Disconnected {
            venue: Venue::Kalshi,
        })
        .await?;
        recorder.flush()?;
        if !acknowledged {
            attempt = attempt.saturating_add(1);
        }
        reconnect = true;
    }
}
