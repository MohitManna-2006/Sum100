//! Polymarket CLOB market-channel parsing.
//!
//! The wire shape here is not Kalshi's, and the differences are the whole
//! reason this is a separate parser rather than a flag on the other one.
//!
//! - A `book` arrives as a JSON *array*, one entry per subscribed token; every
//!   other message is a JSON object. The discriminator is `event_type`.
//! - A `price_change` nests its updates under `price_changes`, so one frame
//!   carries several level updates, for several tokens.
//! - A level update states the **new absolute size** at that price. It is not a
//!   signed change, which is what [`crate::feed::kalshi`] receives, and applying
//!   one as though it were would corrupt the book on the first update.
//! - The server answers the client's `PING` with a bare `PONG` text frame that
//!   is not JSON at all, and it must not be counted as a payload the parser
//!   failed to understand.
//!
//! Each of those is asserted against a recorded production session in
//! `tests/fixtures/polymarket-0.01-tick-sample.ndjson.gz`, whose README states
//! the evidence for the absolute-size reading.
//!
//! # One contract per token
//!
//! A Polymarket market has two tokens, and the venue reports both sides of the
//! same resting order twice: an ask at `p` on the yes token is a bid at
//! `100 - p` on the no token, at identical size. One token therefore carries the
//! whole market, and its two ladders map onto the two halves of
//! [`crate::types::Book`] exactly as Kalshi's do — which is why a 0.01-tick
//! Polymarket market needs no change to the book representation.
//!
//! # Prices and sizes are exact
//!
//! Both arrive as decimal strings and are converted with the same helpers the
//! Kalshi path uses, so there is one definition of a cent in this codebase.
//! A price finer than a cent is rejected rather than rounded: it means the
//! market's tick structure is not one this book can represent, and rounding it
//! would put a quote nobody made in front of the solver.

use crate::{
    feed::FeedEvent,
    metrics::Metrics,
    types::{Cents, ContractId, Contracts, Level, Venue, parse_price_cents, parse_size_contracts},
};

pub use crate::types::TokenSide;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookSnapshot {
    pub contract: ContractId,
    /// The venue's own timestamp, in epoch milliseconds.
    pub venue_ts_ms: u64,
    /// The venue's integrity digest for this book.
    pub hash: String,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

/// One price level replaced outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelSet {
    pub contract: ContractId,
    pub side: TokenSide,
    pub price: Cents,
    /// The new size resting at this price, not a change to it.
    pub size: i64,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// One entry per subscribed token.
    Books(Vec<BookSnapshot>),
    Levels {
        venue_ts_ms: u64,
        changes: Vec<LevelSet>,
    },
}

impl Parsed {
    /// Flatten into the events the book store applies.
    ///
    /// One frame becomes several events, because the venue batches every token
    /// it has news for into one message. A snapshot needs a sequence number the
    /// venue does not supply, so the caller provides one: it is a local count of
    /// snapshots for that book, used to satisfy the book store's rebase and
    /// never to detect a gap, which this venue gives no way to do.
    pub fn into_events(self, snapshot_seq: impl Fn(ContractId) -> u64) -> Vec<FeedEvent> {
        match self {
            Parsed::Books(books) => books
                .into_iter()
                .map(|book| FeedEvent::Snapshot {
                    seq: snapshot_seq(book.contract),
                    contract: book.contract,
                    // A bid for this outcome, and an ask stored as a bid on its
                    // complement: the same identity the two ladders encode.
                    yes: book.bids,
                    no: book
                        .asks
                        .into_iter()
                        .map(|level| Level {
                            price: 100 - level.price,
                            size: level.size,
                        })
                        .collect(),
                    venue_ts_ms: Some(book.venue_ts_ms),
                })
                .collect(),
            Parsed::Levels {
                venue_ts_ms,
                changes,
            } => changes
                .into_iter()
                .map(|change| FeedEvent::LevelSet {
                    contract: change.contract,
                    side: change.side,
                    price: change.price,
                    size: change.size,
                    venue_ts_ms,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct RawLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct RawBook {
    asset_id: String,
    timestamp: String,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
}

#[derive(Deserialize)]
struct RawPriceChange {
    asset_id: String,
    price: String,
    size: String,
    side: String,
    #[serde(default)]
    hash: String,
}

#[derive(Deserialize)]
struct RawPriceChangeFrame {
    price_changes: Vec<RawPriceChange>,
    timestamp: String,
}

#[derive(Deserialize)]
struct EventType {
    event_type: String,
}

pub struct Parser {
    pub contracts: Contracts,
    pub metrics: Metrics,
    /// The venue whose wire format this parser speaks. Read rather than
    /// re-asserted by callers, matching [`crate::feed::kalshi::Parser`].
    pub venue: Venue,
}

impl Parser {
    pub fn new(tokens: &[String]) -> Result<Self> {
        let venue = Venue::Polymarket;
        let mut contracts = Contracts::default();
        for token in tokens {
            contracts.intern(venue, token)?;
        }
        Ok(Self {
            contracts,
            metrics: Metrics::default(),
            venue,
        })
    }

    /// Parse one frame. `None` means the frame carried no book information,
    /// which covers heartbeats and the message types this feed ignores.
    pub fn parse(&mut self, raw: &str, receipt: u64) -> Option<Parsed> {
        self.metrics.parse_attempts += 1;
        match self.parse_inner(raw, receipt) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.metrics.parse_errors += 1;
                tracing::warn!(%error, raw, "venue payload rejected");
                None
            }
        }
    }

    fn parse_inner(&mut self, raw: &str, receipt: u64) -> Result<Option<Parsed>> {
        let trimmed = raw.trim();
        // The heartbeat reply is a bare token, not JSON. Treating it as a
        // malformed payload would report a permanent parse-error rate on a
        // perfectly healthy feed.
        if trimmed.eq_ignore_ascii_case("pong") || trimmed.eq_ignore_ascii_case("ping") {
            return Ok(None);
        }
        if trimmed.is_empty() {
            return Ok(None);
        }

        // A book arrives as an array and everything else as an object, so the
        // container is part of the discriminator.
        if trimmed.starts_with('[') {
            let books: Vec<RawBook> = serde_json::from_str(trimmed)?;
            let mut out = Vec::with_capacity(books.len());
            for book in books {
                out.extend(self.book(book)?);
            }
            return Ok((!out.is_empty()).then_some(Parsed::Books(out)));
        }

        let EventType { event_type } = serde_json::from_str(trimmed)?;
        match event_type.as_str() {
            "book" => {
                let book: RawBook = serde_json::from_str(trimmed)?;
                Ok(self
                    .book(book)?
                    .map(|snapshot| Parsed::Books(vec![snapshot])))
            }
            "price_change" => {
                let frame: RawPriceChangeFrame = serde_json::from_str(trimmed)?;
                let venue_ts_ms: u64 = frame
                    .timestamp
                    .parse()
                    .context("price_change timestamp is not epoch milliseconds")?;
                let mut changes = Vec::with_capacity(frame.price_changes.len());
                for change in frame.price_changes {
                    // A change for a token this parser did not subscribe to is
                    // not an error: one market's frame names both its tokens,
                    // and only one of them need be subscribed.
                    let Some(contract) = self.contracts.get(self.venue, &change.asset_id) else {
                        continue;
                    };
                    let mut discarded = 0;
                    changes.push(LevelSet {
                        contract,
                        side: match change.side.as_str() {
                            "BUY" => TokenSide::Bid,
                            "SELL" => TokenSide::Ask,
                            other => bail!("unknown order side {other:?}"),
                        },
                        price: parse_price_cents(&change.price)?,
                        size: parse_size_contracts(&change.size, &mut discarded)?,
                        hash: change.hash,
                    });
                    self.metrics.discarded_size_hundredths = self
                        .metrics
                        .discarded_size_hundredths
                        .saturating_add(discarded);
                }
                self.metrics.observe_latency(receipt, venue_ts_ms);
                Ok((!changes.is_empty()).then_some(Parsed::Levels {
                    venue_ts_ms,
                    changes,
                }))
            }
            // Known, and deliberately not book state. A trade print says a trade
            // happened, not that resting liquidity moved, and a tick change
            // only matters for markets this engine already refuses to carry.
            "last_trade_price" | "tick_size_change" => Ok(None),
            other => {
                self.metrics.unknown_messages += 1;
                tracing::debug!(kind = other, "skipping unknown message type");
                Ok(None)
            }
        }
    }

    /// `None` when the token is not one this parser carries. A book frame names
    /// every token of the market, and only one of them need be subscribed, so
    /// the others are skipped on the same terms as in a level update.
    fn book(&mut self, raw: RawBook) -> Result<Option<BookSnapshot>> {
        let Some(contract) = self.contracts.get(self.venue, &raw.asset_id) else {
            return Ok(None);
        };
        let venue_ts_ms: u64 = raw
            .timestamp
            .parse()
            .context("book timestamp is not epoch milliseconds")?;
        let mut discarded = 0;
        let mut ladder = |levels: Vec<RawLevel>| -> Result<Vec<Level>> {
            levels
                .into_iter()
                .map(|level| {
                    Ok(Level {
                        price: parse_price_cents(&level.price)?,
                        size: parse_size_contracts(&level.size, &mut discarded)?,
                    })
                })
                .collect()
        };
        let bids = ladder(raw.bids)?;
        let asks = ladder(raw.asks)?;
        self.metrics.discarded_size_hundredths = self
            .metrics
            .discarded_size_hundredths
            .saturating_add(discarded);
        Ok(Some(BookSnapshot {
            contract,
            venue_ts_ms,
            hash: raw.hash,
            bids,
            asks,
        }))
    }
}
