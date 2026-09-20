//! Offline [`Feed`] over a recorded daily file. Each venue payload goes through
//! the same [`Parser::parse`] call the live feed makes, with the recorded
//! receipt time; control envelopes reproduce `Disconnected` and `Resubscribed`
//! exactly where the live feed emitted them. No credentials, no network.
//!
//! A daily file can hold several process runs. Each run begins with a
//! `session_started` control envelope; records before the first marker (files
//! recorded before markers existed) form one unmarked session.
use super::{Feed, FeedEvent, kalshi::Parser};
use crate::{
    clock::ReplayClock,
    metrics::Metrics,
    record::{CONTROL_KIND, Control, Record, read_records},
    types::Venue,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    path::Path,
    pin::Pin,
    time::Duration,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pace {
    /// Sleep recorded inter-arrival gaps, for latency measurement.
    Realtime,
    /// No sleeping, for regression runs.
    #[default]
    Max,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayOptions {
    /// Required for an unmarked session; must match the marker otherwise.
    pub tickers: Option<Vec<String>>,
    pub pace: Pace,
    /// 1-based session index; required when the file holds several.
    pub session: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub index: usize,
    /// `None` for records written before session markers existed.
    pub environment: Option<String>,
    pub tickers: Option<Vec<String>>,
    pub first_received_at_ms: u64,
    pub records: u64,
}

/// Session boundaries in file order. A full streaming pass, which also proves
/// the whole file decodes before replay yields anything.
pub fn sessions(path: &Path) -> Result<Vec<SessionInfo>> {
    let mut sessions: Vec<SessionInfo> = Vec::new();
    for record in read_records(path)? {
        let record = record?;
        let marker = match record.control()? {
            Some(Control::SessionStarted {
                environment,
                tickers,
            }) => Some((environment, tickers)),
            _ => None,
        };
        if marker.is_some() || sessions.is_empty() {
            let (environment, tickers) = marker.unzip();
            sessions.push(SessionInfo {
                index: sessions.len() + 1,
                environment,
                tickers,
                first_received_at_ms: record.received_at_ms,
                records: 0,
            });
        }
        if let Some(session) = sessions.last_mut() {
            session.records += 1;
        }
    }
    Ok(sessions)
}

pub struct ReplayFeed {
    records: Box<dyn Iterator<Item = Result<Record>> + Send>,
    parser: Parser,
    tickers: Vec<String>,
    clock: ReplayClock,
    pace: Pace,
    selected: usize,
    /// Session of the most recently read record; 0 before the first record.
    current: usize,
    pending: VecDeque<FeedEvent>,
    /// Monotonic start instant and the receipt time it corresponds to.
    anchor: Option<(tokio::time::Instant, u64)>,
    error: Option<anyhow::Error>,
    done: bool,
}

impl Feed for ReplayFeed {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>> {
        Box::pin(self.next_event())
    }

    /// Read straight off the parser this feed owns. Replay has no worker task to
    /// publish through, and the counts are a function of the recorded bytes, so
    /// a dashboard driven by a replay shows exactly what the live run showed.
    fn metrics(&self) -> Option<Metrics> {
        Some(self.parser.metrics)
    }
}

impl ReplayFeed {
    pub fn open(path: &Path, options: ReplayOptions) -> Result<Self> {
        let sessions = sessions(path).with_context(|| format!("reading {}", path.display()))?;
        ensure!(
            !sessions.is_empty(),
            "{} contains no records",
            path.display()
        );
        let selected = match options.session {
            Some(index) => {
                ensure!(
                    (1..=sessions.len()).contains(&index),
                    "session {index} out of range; {} has {} session(s)",
                    path.display(),
                    sessions.len()
                );
                index
            }
            None if sessions.len() == 1 => 1,
            None => bail!(
                "{} holds {} sessions; choose one with --session:\n{}",
                path.display(),
                sessions.len(),
                describe(&sessions)
            ),
        };
        let session = &sessions[selected - 1];
        let tickers = match (options.tickers, &session.tickers) {
            (Some(given), Some(recorded)) => {
                ensure!(
                    given.iter().collect::<BTreeSet<_>>() == recorded.iter().collect(),
                    "--tickers {given:?} differ from the recorded session tickers {recorded:?}"
                );
                given
            }
            (Some(given), None) => given,
            (None, Some(recorded)) => recorded.clone(),
            (None, None) => {
                bail!("session {selected} predates session markers; pass --tickers as recorded")
            }
        };
        ensure!(
            !tickers.is_empty() && tickers.iter().all(|t| !t.trim().is_empty()),
            "at least one nonempty ticker is required"
        );
        Ok(Self {
            records: Box::new(read_records(path)?),
            parser: Parser::new(&tickers)?,
            tickers,
            clock: ReplayClock::new(),
            pace: options.pace,
            selected,
            current: 0,
            pending: VecDeque::new(),
            anchor: None,
            error: None,
            done: false,
        })
    }

    /// Tickers in parser interning order; build the book store from these.
    pub fn tickers(&self) -> &[String] {
        &self.tickers
    }

    /// The venue this recording came from, which is the one its parser speaks.
    pub fn venue(&self) -> Venue {
        self.parser.venue
    }

    /// Advanced to each event's receipt time before the event is yielded.
    pub fn clock(&self) -> ReplayClock {
        self.clock.clone()
    }

    /// Parser metrics, or the error that ended the stream early.
    pub fn finish(self) -> Result<Metrics> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.parser.metrics),
        }
    }

    async fn next_event(&mut self) -> Option<FeedEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            match self.read_batch() {
                Ok(Some(received_at_ms)) => {
                    self.pace_to(received_at_ms).await;
                    self.clock.advance_to(received_at_ms);
                }
                Ok(None) => self.done = true,
                Err(error) => {
                    tracing::error!(%error, "replay stopped");
                    self.error = Some(error);
                    self.pending.clear();
                    self.done = true;
                }
            }
        }
    }

    /// Read records until one yields events; returns their receipt time.
    fn read_batch(&mut self) -> Result<Option<u64>> {
        while let Some(record) = self.records.next() {
            let record = record?;
            let control = record.control()?;
            if matches!(control, Some(Control::SessionStarted { .. })) || self.current == 0 {
                self.current += 1;
            }
            if self.current < self.selected {
                continue;
            }
            if self.current > self.selected {
                return Ok(None);
            }
            self.decode(&record, control)?;
            if !self.pending.is_empty() {
                return Ok(Some(record.received_at_ms));
            }
        }
        Ok(None)
    }

    fn decode(&mut self, record: &Record, control: Option<Control>) -> Result<()> {
        match control {
            Some(Control::SessionStarted { .. } | Control::Reconnected { .. }) => {}
            Some(Control::Disconnected { .. }) => self.pending.push_back(FeedEvent::Disconnected {
                venue: self.parser.venue,
            }),
            Some(Control::Resubscribed { tickers }) => {
                // The live feed counts one reconnection per envelope it writes
                // here, so counting one per envelope read reproduces the figure
                // the live run reported rather than leaving replay at zero.
                self.parser.metrics.reconnections += 1;
                for ticker in tickers {
                    let contract = self
                        .parser
                        .contracts
                        .get(self.parser.venue, &ticker)
                        .with_context(|| {
                            format!("record {}: resubscribed unknown {ticker}", record.sequence)
                        })?;
                    self.pending.push_back(FeedEvent::Resubscribed { contract });
                }
            }
            None => {
                ensure!(
                    record.kind != CONTROL_KIND,
                    "control envelope did not decode"
                );
                self.parser.metrics.messages_received += 1;
                match record.kind.as_str() {
                    // Identical call to the live feed: raw text plus receipt time.
                    "text" => self
                        .pending
                        .extend(self.parser.parse(&record.raw, record.received_at_ms)),
                    // The live feed records these but never parses them.
                    "binary_base64" | "ping_base64" | "pong_base64" | "close_base64" => {}
                    kind => bail!("record {}: unknown envelope kind {kind:?}", record.sequence),
                }
            }
        }
        Ok(())
    }

    async fn pace_to(&mut self, received_at_ms: u64) {
        if self.pace == Pace::Max {
            return;
        }
        // Monotonic scheduling only: anchoring avoids accumulating sleep
        // overshoot. It never reaches an event or the replay clock.
        let (start, first_ms) = *self
            .anchor
            .get_or_insert_with(|| (tokio::time::Instant::now(), received_at_ms));
        let offset = Duration::from_millis(received_at_ms.saturating_sub(first_ms));
        tokio::time::sleep_until(start + offset).await;
    }
}

fn describe(sessions: &[SessionInfo]) -> String {
    sessions
        .iter()
        .map(|s| match &s.tickers {
            Some(tickers) => format!(
                "  {}: {} records from {} ms, {} {:?}",
                s.index,
                s.records,
                s.first_received_at_ms,
                s.environment.as_deref().unwrap_or("?"),
                tickers
            ),
            None => format!(
                "  {}: {} records from {} ms, unmarked (recorded before session markers)",
                s.index, s.records, s.first_received_at_ms
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}
