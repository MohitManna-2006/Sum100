//! Receipt envelopes preserve raw payloads independently of the venue parser.
//! Each flush closes a gzip member so the durable prefix is readable after a
//! crash. Readers must support concatenated members (MultiGzDecoder).
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

pub struct Recorder {
    // Hold an OS lock for this venue/output directory across daily rotations.
    _lock: File,
    root: PathBuf,
    day: Option<String>,
    sequence: u64,
    encoder: Option<GzEncoder<File>>,
    path: Option<PathBuf>,
}
impl Recorder {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        std::fs::create_dir_all(root.as_ref())?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.as_ref().join(".kalshi.lock"))?;
        lock.try_lock()
            .context("another recorder owns this output directory")?;
        Ok(Self {
            _lock: lock,
            root: root.as_ref().to_owned(),
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
            let path = self.root.join(format!("kalshi-{day}.ndjson.gz"));
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
    pub fn write(&mut self, received_at_ms: u64, kind: &str, raw: &str) -> Result<u64> {
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

pub fn read_records(path: &Path) -> Result<impl Iterator<Item = Result<Record>>> {
    let reader = BufReader::new(MultiGzDecoder::new(File::open(path)?));
    Ok(reader.lines().map(|line| {
        let line = line?;
        ensure!(!line.is_empty(), "blank line in recording");
        Ok(serde_json::from_str(&line)?)
    }))
}
