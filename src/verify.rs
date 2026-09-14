//! Deterministic fingerprint of a run: final book state per contract plus the
//! gap log. Timestamps are excluded by construction, so a live `dump` and a
//! `replay` of its recording produce byte-identical digests.
use crate::{
    book::{Applied, BookStore},
    feed::FeedEvent,
    types::{Book, BookState, ContractId, Venue},
};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

pub const DIGEST_HEADER: &str = "# sum100 state digest v1 (timestamps excluded)";

/// Ordered record of every event that changes resync state. Entries carry the
/// 1-based position of the triggering event in the feed stream, never a time.
#[derive(Debug, Default, Clone)]
pub struct GapLog {
    events_seen: u64,
    entries: Vec<String>,
}

impl GapLog {
    /// The single event-application step shared by the live and replay
    /// drivers. On [`Applied::Gap`] the resync request is counted here; a live
    /// driver must additionally ask its feed to reconnect.
    pub fn apply(&mut self, store: &mut BookStore, event: &FeedEvent) -> Applied {
        self.events_seen += 1;
        let position = self.events_seen;
        let was_resyncing = match event {
            FeedEvent::Snapshot { contract, .. } => store
                .get(*contract)
                .is_some_and(|book| book.state == BookState::Resyncing),
            _ => false,
        };
        let applied = store.apply(event);
        let entry = match (&applied, event) {
            (Applied::Gap { expected, got }, FeedEvent::Delta { contract, .. }) => {
                store.note_resync_request();
                Some(format!(
                    "{position} gap {} expected={expected} got={got}",
                    ticker(store, *contract)
                ))
            }
            (Applied::Invalidated, FeedEvent::Disconnected { venue }) => {
                Some(format!("{position} disconnected {}", venue_name(*venue)))
            }
            (Applied::Invalidated, FeedEvent::Resubscribed { contract }) => Some(format!(
                "{position} resubscribed {}",
                ticker(store, *contract)
            )),
            (Applied::Snapshot(contract), FeedEvent::Snapshot { seq, .. }) if was_resyncing => {
                Some(format!(
                    "{position} resynced {} seq={seq}",
                    ticker(store, *contract)
                ))
            }
            _ => None,
        };
        self.entries.extend(entry);
        applied
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn events_seen(&self) -> u64 {
        self.events_seen
    }
}

fn ticker(store: &BookStore, contract: ContractId) -> &str {
    store
        .contracts()
        .resolve(contract)
        .map_or("?", |(_, ticker)| ticker.as_str())
}

fn venue_name(venue: Venue) -> &'static str {
    match venue {
        Venue::Kalshi => "kalshi",
        Venue::Polymarket => "polymarket",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Canonical text of one book: wire sides, sequence, and state. Excludes
/// `updated_at_ms`. Only nonzero levels appear; the dense arrays are otherwise
/// zero, so this is the complete state.
pub fn canonical_book(book: &Book, ticker: &str) -> String {
    let state = match book.state {
        BookState::Uninitialized => "uninitialized",
        BookState::Resyncing => "resyncing",
        BookState::Live => "live",
    };
    let side = |size_at: &dyn Fn(i64) -> Option<i64>| {
        (0..=100)
            .filter_map(|price| {
                size_at(price)
                    .filter(|size| *size != 0)
                    .map(|size| format!("{price}:{size}"))
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "ticker={ticker} state={state} seq={} yes={} no={}",
        book.seq,
        side(&|p| book.yes_size_at(p)),
        side(&|p| book.no_size_at(p)),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDigest {
    /// (ticker, sha256 of canonical book), in store order.
    pub books: Vec<(String, String)>,
    /// sha256 over gap log entries, each terminated by LF.
    pub gaps: String,
    pub gap_entries: Vec<String>,
    /// Book-store counters. Deterministic but informational: not verified.
    pub metrics: String,
}

impl StateDigest {
    pub fn new(store: &BookStore, log: &GapLog) -> Self {
        let books = store
            .books()
            .iter()
            .map(|book| {
                let ticker = ticker(store, book.contract_id).to_owned();
                let hash = sha256_hex(canonical_book(book, &ticker).as_bytes());
                (ticker, hash)
            })
            .collect();
        let mut gap_text = String::new();
        for entry in log.entries() {
            gap_text.push_str(entry);
            gap_text.push('\n');
        }
        let m = &store.metrics;
        Self {
            books,
            gaps: sha256_hex(gap_text.as_bytes()),
            gap_entries: log.entries().to_vec(),
            metrics: format!(
                "events={} snapshots_applied={} deltas_applied={} deltas_skipped_not_live={} \
                 sequence_gaps={} resync_requests={} negative_level_clamps={} \
                 crossed_books_observed={}",
                log.events_seen(),
                m.snapshots_applied,
                m.deltas_applied,
                m.deltas_skipped_not_live,
                m.sequence_gaps,
                m.resync_requests,
                m.negative_level_clamps,
                m.crossed_books_observed,
            ),
        }
    }

    /// Line format read back by [`Expected::parse`].
    pub fn render(&self) -> String {
        let mut out = format!("{DIGEST_HEADER}\n");
        for (ticker, hash) in &self.books {
            out.push_str(&format!("book {ticker} {hash}\n"));
        }
        out.push_str(&format!("gaps {}\n", self.gaps));
        for entry in &self.gap_entries {
            out.push_str(&format!("gap {entry}\n"));
        }
        out.push_str(&format!("metrics {}\n", self.metrics));
        out
    }
}

/// Expected hashes for `replay --verify`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Expected {
    pub books: BTreeMap<String, String>,
    pub gaps: Option<String>,
}

fn normalize_hash(hash: &str) -> Result<String> {
    let hash = hash.trim().to_ascii_lowercase();
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected a 64-digit sha256 hex value, got {hash:?}"
    );
    Ok(hash)
}

impl Expected {
    /// Read `book` and `gaps` lines from a rendered digest. `gap`, `metrics`,
    /// comments, and blank lines are ignored.
    pub fn parse(text: &str) -> Result<Self> {
        let mut expected = Self::default();
        for (number, line) in text.lines().enumerate() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                [] => {}
                [first, ..] if first.starts_with('#') => {}
                ["gap" | "metrics", ..] => {}
                ["book", ticker, hash] => expected.add_book(ticker, hash)?,
                ["gaps", hash] => expected.set_gaps(hash)?,
                _ => bail!("digest line {} is malformed: {line:?}", number + 1),
            }
        }
        Ok(expected)
    }

    pub fn add_book(&mut self, ticker: &str, hash: &str) -> Result<()> {
        let previous = self.books.insert(ticker.to_owned(), normalize_hash(hash)?);
        ensure!(previous.is_none(), "duplicate expectation for {ticker}");
        Ok(())
    }

    /// Parse a command-line `TICKER=SHA256` pair.
    pub fn add_book_arg(&mut self, arg: &str) -> Result<()> {
        let (ticker, hash) = arg
            .rsplit_once('=')
            .context("--expect-book takes TICKER=SHA256")?;
        self.add_book(ticker, hash)
    }

    pub fn set_gaps(&mut self, hash: &str) -> Result<()> {
        ensure!(self.gaps.is_none(), "duplicate gap log expectation");
        self.gaps = Some(normalize_hash(hash)?);
        Ok(())
    }

    /// Every replayed contract and the gap log must be covered and match.
    pub fn check(&self, actual: &StateDigest) -> Vec<String> {
        let mut failures = Vec::new();
        for (ticker, hash) in &actual.books {
            match self.books.get(ticker) {
                Some(want) if want == hash => {}
                Some(want) => failures.push(format!("book {ticker}: expected {want}, got {hash}")),
                None => failures.push(format!("book {ticker}: no expected value supplied")),
            }
        }
        for ticker in self.books.keys() {
            if !actual.books.iter().any(|(t, _)| t == ticker) {
                failures.push(format!("book {ticker}: expected but not in replay"));
            }
        }
        match &self.gaps {
            Some(want) if *want == actual.gaps => {}
            Some(want) => failures.push(format!("gaps: expected {want}, got {}", actual.gaps)),
            None => failures.push("gaps: no expected value supplied".into()),
        }
        failures
    }
}
