//! Receipt envelopes preserve raw payloads independently of the venue parser.
//! Each flush closes a gzip member so the durable prefix is readable after a
//! crash. Readers must support concatenated members (MultiGzDecoder).
//!
//! Feed lifecycle events that never arrive as venue bytes (disconnect,
//! reconnect, resubscribe) are written into the same stream as `kind:
//! "control"` envelopes so replay takes the same resync path the live run did.
//! The tag is the envelope `kind`, never the payload content; files recorded
//! before control envelopes existed parse unchanged and simply contain none.
use crate::types::Venue;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    pub received_at_ms: u64,
    pub sequence: u64,
    pub kind: String,
    /// Text is JSON-escaped as a string, never deserialized/re-serialized.
    /// Non-text WebSocket payloads use base64; kind identifies the encoding.
    pub raw: String,
}

/// Envelope kind reserved for [`Control`]; no WebSocket frame uses it.
pub const CONTROL_KIND: &str = "control";

/// Feed lifecycle event, JSON-encoded into the envelope `raw` string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Control {
    /// Once per feed process, before the first connection attempt. Marks
    /// session boundaries inside a daily file that several runs appended to.
    SessionStarted {
        environment: String,
        tickers: Vec<String>,
    },
    /// Written immediately before the feed emits `FeedEvent::Disconnected`.
    Disconnected { reason: String },
    /// A later handshake succeeded; emits no feed event.
    Reconnected { attempt: u32 },
    /// Written immediately before the feed emits `FeedEvent::Resubscribed`
    /// for each ticker, in this order.
    Resubscribed { tickers: Vec<String> },
}

impl Record {
    /// `Some` for a control envelope, `None` for a venue payload.
    pub fn control(&self) -> Result<Option<Control>> {
        if self.kind != CONTROL_KIND {
            return Ok(None);
        }
        let control = serde_json::from_str(&self.raw)
            .with_context(|| format!("malformed control envelope {}", self.sequence))?;
        Ok(Some(control))
    }
}

pub struct Recorder {
    // Hold an OS lock for this venue/output directory across daily rotations.
    _lock: File,
    root: PathBuf,
    venue: Venue,
    day: Option<String>,
    sequence: u64,
    encoder: Option<GzEncoder<File>>,
    path: Option<PathBuf>,
}
impl Recorder {
    /// The venue names both the daily file and the lock, so two venues can
    /// record into one directory at once and a file says which venue wrote it.
    /// Kalshi's names are unchanged, so existing corpora keep resuming.
    pub fn new(root: impl AsRef<Path>, venue: Venue) -> Result<Self> {
        std::fs::create_dir_all(root.as_ref())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.as_ref().join(format!(".{}.lock", venue.name())))?;
        lock.try_lock()
            .context("another recorder owns this output directory")?;
        Ok(Self {
            _lock: lock,
            root: root.as_ref().to_owned(),
            venue,
            day: None,
            sequence: 0,
            encoder: None,
            path: None,
        })
    }
    fn select_day(&mut self, ts: u64) -> Result<()> {
        let time = DateTime::<Utc>::from_timestamp_millis(i64::try_from(ts)?)
            .context("invalid receipt timestamp")?;
        let day = time.format("%Y-%m-%d").to_string();
        if self.day.as_ref() != Some(&day) {
            self.flush()?;
            let path = self
                .root
                .join(format!("{}-{day}.ndjson.gz", self.venue.name()));
            if path.exists() {
                // Validate the existing corpus and resume above its last sequence.
                // Refuse a damaged tail rather than append behind unreadable bytes.
                for record in read_records(&path)? {
                    self.sequence = self.sequence.max(record?.sequence);
                }
            }
            self.path = Some(path);
            self.day = Some(day);
        }
        Ok(())
    }
    /// Record one venue payload. `kind` names the WebSocket frame encoding.
    pub fn write(&mut self, received_at_ms: u64, kind: &str, raw: &str) -> Result<u64> {
        ensure!(
            kind != CONTROL_KIND,
            "control envelopes must use write_control"
        );
        self.append(received_at_ms, kind, raw)
    }

    pub fn write_control(&mut self, received_at_ms: u64, control: &Control) -> Result<u64> {
        self.append(
            received_at_ms,
            CONTROL_KIND,
            &serde_json::to_string(control)?,
        )
    }

    fn append(&mut self, received_at_ms: u64, kind: &str, raw: &str) -> Result<u64> {
        self.select_day(received_at_ms)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("record sequence overflow")?;
        let record = Record {
            received_at_ms,
            sequence: self.sequence,
            kind: kind.to_owned(),
            raw: if kind == "text" {
                raw.strip_suffix('\n').unwrap_or(raw).to_owned()
            } else {
                raw.to_owned()
            },
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        if self.encoder.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path.as_ref().context("missing recording path")?)?;
            self.encoder = Some(GzEncoder::new(file, Compression::default()));
        }
        self.encoder
            .as_mut()
            .context("missing gzip encoder")?
            .write_all(&bytes)?;
        Ok(bytes.len() as u64)
    }
    pub fn flush(&mut self) -> Result<()> {
        if let Some(encoder) = self.encoder.take() {
            encoder.finish()?.sync_data()?;
        }
        Ok(())
    }
}

pub fn read_records(path: &Path) -> Result<impl Iterator<Item = Result<Record>> + Send + use<>> {
    let reader = BufReader::new(MultiGzDecoder::new(File::open(path)?));
    Ok(reader.lines().map(|line| {
        let line = line?;
        ensure!(!line.is_empty(), "blank line in recording");
        Ok(serde_json::from_str(&line)?)
    }))
}
