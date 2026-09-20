//! Throwaway stage-1 probe: connect to Kalshi, subscribe to one public channel
//! for one ticker, and print every frame verbatim.
//!
//! This binary exists to discover the real wire format before any abstraction is
//! built on top of it. It is deliberately flat and dependency-light: no feed
//! trait, no recorder, no reconnection. Delete it once `src/feed/kalshi.rs`
//! supersedes it.
//!
//! Defaults to the demo environment. Pass `--prod` to reach production.

use std::env;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::pss::SigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use sha2::Sha256;
use sum100::clock::{Clock, WallClock};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

/// Path signed for the WebSocket upgrade. Per Kalshi's docs the signature covers
/// the path from the API root only, never the host and never a query string, so
/// the same string is correct for demo and production.
const WS_PATH: &str = "/trade-api/ws/v2";

const WS_DEMO: &str = "wss://external-api-ws.demo.kalshi.co/trade-api/ws/v2";
const WS_PROD: &str = "wss://external-api-ws.kalshi.com/trade-api/ws/v2";

/// One hardcoded market for the probe. A Bitcoin daily strike is the right first
/// target: it trades continuously with a tight spread, so a short probe run
/// actually exercises the message types instead of sitting idle, and its event is
/// a genuine mutually exclusive and exhaustive strike ladder of the shape phase 5
/// hunts for.
const DEFAULT_TICKER: &str = "KXBTCD-26SEP1417-T76999.99";

/// Load the RSA private key, accepting both PEM encodings Kalshi may hand out.
///
/// Kalshi's console issues a PKCS#1 `RSA PRIVATE KEY` block, but keys that have
/// been round-tripped through other tooling often come back as PKCS#8
/// `PRIVATE KEY`. Accepting both here avoids a confusing startup failure that
/// has nothing to do with the key actually being wrong.
fn load_key(path: &str) -> Result<RsaPrivateKey> {
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("reading KALSHI_PRIVATE_KEY_PATH at {path}"))?;

    if let Ok(key) = RsaPrivateKey::from_pkcs1_pem(&pem) {
        return Ok(key);
    }
    RsaPrivateKey::from_pkcs8_pem(&pem)
        .context("private key is neither a valid PKCS#1 nor PKCS#8 PEM RSA key")
}

/// Sign `timestamp + method + path` with RSA-PSS over SHA-256, base64 encoded.
///
/// PSS uses a random salt, so this is intentionally non-deterministic: signing
/// the same message twice yields different bytes, and both verify.
fn sign(key: &RsaPrivateKey, timestamp_ms: i64, method: &str, path: &str) -> String {
    let msg = format!("{timestamp_ms}{method}{path}");
    let signing_key = SigningKey::<Sha256>::new(key.clone());
    // `rsa` is built against the rand_core 0.6 generation, so the RNG must come
    // from its own re-export; the top-level `rand` 0.10 traits do not apply here.
    let mut rng = rsa::rand_core::OsRng;
    let sig = signing_key.sign_with_rng(&mut rng, msg.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let prod = args.iter().any(|a| a == "--prod");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let ticker = positional
        .first()
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_TICKER.to_string());
    // Throwaway convenience: let the probe target any channel set so the real
    // wire shape of each one can be captured before stage 2 commits to a parser.
    let channels: Vec<String> = positional
        .get(1)
        .map(|s| s.split(',').map(|c| c.trim().to_string()).collect())
        .unwrap_or_else(|| vec!["ticker".to_string(), "trade".to_string()]);

    let url = if prod {
        eprintln!("WARNING: connecting to PRODUCTION. This probe is read-only.");
        WS_PROD
    } else {
        WS_DEMO
    };

    // Credentials come from the environment only. A key path committed to the
    // repo is a leak waiting to happen, so there is deliberately no file or
    // config fallback here.
    let key_id = env::var("KALSHI_KEY_ID").context("KALSHI_KEY_ID must be set")?;
    let key_path =
        env::var("KALSHI_PRIVATE_KEY_PATH").context("KALSHI_PRIVATE_KEY_PATH must be set")?;

    // Fail on a bad key here, at startup, rather than several frames into a
    // session when it is far harder to attribute.
    let key = load_key(&key_path)?;

    let timestamp_ms = i64::try_from(WallClock.now_ms())?;
    let signature = sign(&key, timestamp_ms, "GET", WS_PATH);

    let mut request = url.into_client_request()?;
    {
        let headers = request.headers_mut();
        headers.insert("KALSHI-ACCESS-KEY", HeaderValue::from_str(&key_id)?);
        headers.insert(
            "KALSHI-ACCESS-SIGNATURE",
            HeaderValue::from_str(&signature)?,
        );
        headers.insert(
            "KALSHI-ACCESS-TIMESTAMP",
            HeaderValue::from_str(&timestamp_ms.to_string())?,
        );
    }

    eprintln!("connecting to {url}");
    let (mut ws, response) = match tokio_tungstenite::connect_async(request).await {
        Ok(ok) => ok,
        Err(e) => bail!("websocket handshake failed: {e}"),
    };
    eprintln!("connected, http status {}", response.status());

    let subscribe = serde_json::json!({
        "id": 1,
        "cmd": "subscribe",
        "params": {
            "channels": channels,
            "market_tickers": [ticker],
        }
    });
    let payload = serde_json::to_string(&subscribe)?;
    eprintln!("sending subscribe: {payload}");
    ws.send(Message::Text(payload.into())).await?;

    // Print frames verbatim until the peer closes. Kalshi pings every 10s and
    // tokio-tungstenite answers pongs automatically, so no keepalive here.
    while let Some(frame) = ws.next().await {
        match frame {
            Ok(Message::Text(t)) => println!("{t}"),
            Ok(Message::Binary(b)) => println!("<binary {} bytes>", b.len()),
            Ok(Message::Close(c)) => {
                eprintln!("server closed: {c:?}");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("stream error: {e}");
                break;
            }
        }
    }

    eprintln!("disconnected");
    Ok(())
}
