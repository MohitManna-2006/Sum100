//! Public GET endpoints only. Both lists follow cursor pagination.
use super::kalshi::Environment;
use crate::clock::Clock;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc, time::Duration};
#[derive(Debug, Deserialize, Serialize)]
pub struct Event {
    pub event_ticker: String,
    pub series_ticker: String,
    pub title: String,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Market {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
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
    env: Environment,
    clock: Arc<dyn Clock>,
}
impl Rest {
    pub fn new(env: Environment, clock: Arc<dyn Clock>) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()?,
            env,
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
                .get(format!("{}{path}", self.env.rest_url()))
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
    pub async fn series_markets(&self, series: &str) -> Result<Vec<Market>> {
        let mut markets = Vec::new();
        for event in self.events(series).await? {
            markets.extend(self.markets(&event.event_ticker).await?);
        }
        Ok(markets)
    }
}
