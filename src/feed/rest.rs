//! Public GET endpoints only. Both lists follow cursor pagination.
use super::kalshi::Environment;
use crate::clock::Clock;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc, time::Duration};
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    pub event_ticker: String,
    #[serde(default)]
    pub series_ticker: String,
    #[serde(default)]
    pub title: String,
    /// The venue's own word on whether exactly one market under this event
    /// resolves yes. This is the authoritative signal for an exhaustive set, and
    /// it is why inference does not have to guess from titles or tickers.
    #[serde(default)]
    pub mutually_exclusive: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Market {
    pub ticker: String,
    pub event_ticker: String,
    #[serde(default)]
    pub title: String,
    /// "greater", "less", "between", "custom", or absent. A ladder is a run of
    /// "greater" rungs under one event.
    #[serde(default)]
    pub strike_type: Option<String>,
    /// Threshold for a "greater" rung, in the market's own units (dollars for
    /// a price market). Only used for ordering, never for money.
    #[serde(default)]
    pub floor_strike: Option<f64>,
    #[serde(default)]
    pub cap_strike: Option<f64>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub close_time: Option<String>,
    #[serde(default)]
    pub expiration_time: Option<String>,
}
#[derive(Deserialize)]
struct Events {
    events: Vec<Event>,
    cursor: Option<String>,
}
#[derive(Deserialize)]
struct Markets {
    markets: Vec<Market>,
    cursor: Option<String>,
}
pub struct Rest {
    client: reqwest::Client,
    base_url: String,
    clock: Arc<dyn Clock>,
}
impl Rest {
    pub fn new(env: Environment, clock: Arc<dyn Clock>) -> Result<Self> {
        Self::with_base_url(env, env.rest_url(), clock)
    }

    /// Construct a REST client against an explicit base URL.
    ///
    /// Production code should normally use [`Rest::new`].  Keeping the URL
    /// injectable makes discovery integration tests deterministic and lets a
    /// caller point at a local proxy without changing the Kalshi environment
    /// selection used by the websocket and order clients.
    pub fn with_base_url(
        _env: Environment,
        base_url: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            clock,
        })
    }
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        for attempt in 0..=5u32 {
            let response = self
                .client
                .get(format!("{}{path}", self.base_url))
                .query(query)
                .send()
                .await?;
            let status = response.status();
            if attempt < 5
                && (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| {
                        value.parse::<u64>().ok().or_else(|| {
                            chrono::DateTime::parse_from_rfc2822(value)
                                .ok()
                                .map(|date| {
                                    let now_s = (self.clock.now_ms() / 1000) as i64;
                                    date.timestamp().saturating_sub(now_s).max(0) as u64
                                })
                        })
                    });
                // Do not retry earlier than the server asks. A long pause is
                // reported to the caller instead of tying discovery up indefinitely.
                ensure!(
                    retry_after.is_none_or(|seconds| seconds <= 30),
                    "REST retry delay exceeds 30 seconds; retry discovery later"
                );
                let delay_ms = retry_after
                    .map(|seconds| seconds * 1000)
                    .unwrap_or((1000u64 << attempt) + rand::random::<u64>() % 250);
                tracing::warn!(%status, path, attempt, delay_ms, "REST request will retry");
                drop(response);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }
            return Ok(response.error_for_status()?.json().await?);
        }
        anyhow::bail!("REST retry budget exhausted")
    }
    pub async fn events(&self, series: &str) -> Result<Vec<Event>> {
        let mut result = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        loop {
            let page: Events = self
                .get(
                    "/events",
                    &[
                        ("series_ticker", series),
                        ("limit", "200"),
                        ("cursor", &cursor),
                    ],
                )
                .await?;
            result.extend(page.events);
            cursor = page.cursor.unwrap_or_default();
            if cursor.is_empty() {
                return Ok(result);
            }
            ensure!(
                seen.insert(cursor.clone()),
                "events pagination repeated cursor"
            );
        }
    }
    pub async fn markets(&self, event: &str) -> Result<Vec<Market>> {
        let mut result = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        loop {
            let page: Markets = self
                .get(
                    "/markets",
                    &[
                        ("event_ticker", event),
                        ("limit", "1000"),
                        ("cursor", &cursor),
                    ],
                )
                .await?;
            result.extend(page.markets);
            cursor = page.cursor.unwrap_or_default();
            if cursor.is_empty() {
                return Ok(result);
            }
            ensure!(
                seen.insert(cursor.clone()),
                "markets pagination repeated cursor"
            );
        }
    }
    /// Every market on the venue, one page at a time.
    ///
    /// `status` filters server side; passing an empty string takes everything,
    /// which on Kalshi is tens of thousands of rows. `limit_pages` bounds the
    /// walk so a discovery run cannot turn into an unbounded crawl.
    pub async fn all_markets(&self, status: &str, limit_pages: usize) -> Result<Vec<Market>> {
        ensure!(limit_pages > 0, "markets pagination limit must be positive");
        let mut result = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        for _ in 0..limit_pages {
            let page: Markets = self
                .get(
                    "/markets",
                    &[("limit", "1000"), ("status", status), ("cursor", &cursor)],
                )
                .await?;
            result.extend(page.markets);
            cursor = page.cursor.unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
            ensure!(
                seen.insert(cursor.clone()),
                "markets pagination repeated cursor"
            );
        }
        ensure!(
            cursor.is_empty(),
            "markets discovery reached its {limit_pages}-page limit before the venue was exhausted"
        );
        Ok(result)
    }

    /// Every event on the venue, for the `mutually_exclusive` flag.
    pub async fn all_events(&self, status: &str, limit_pages: usize) -> Result<Vec<Event>> {
        ensure!(limit_pages > 0, "events pagination limit must be positive");
        let mut result = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        for _ in 0..limit_pages {
            let page: Events = self
                .get(
                    "/events",
                    &[("limit", "200"), ("status", status), ("cursor", &cursor)],
                )
                .await?;
            result.extend(page.events);
            cursor = page.cursor.unwrap_or_default();
            if cursor.is_empty() {
                break;
            }
            ensure!(
                seen.insert(cursor.clone()),
                "events pagination repeated cursor"
            );
        }
        ensure!(
            cursor.is_empty(),
            "events discovery reached its {limit_pages}-page limit before the venue was exhausted"
        );
        Ok(result)
    }

    /// Markets for one series, with their event metadata.
    pub async fn series_markets(&self, series: &str) -> Result<Vec<Market>> {
        let mut markets = Vec::new();
        for event in self.events(series).await? {
            markets.extend(self.markets(&event.event_ticker).await?);
        }
        Ok(markets)
    }
}
