//! Book store: apply feed events to in-memory books with sequence continuity.
//!
//! Sequence numbers are scoped to the venue subscription, not per contract. A
//! gap on any ticker invalidates every live book *on that venue*, and no book
//! on any other: two venues number their own subscriptions from their own
//! handshakes, so treating the streams as one would read every alternating
//! message as a discontinuity. The store never calls back into the feed; on
//! [`Applied::Gap`] the driver must request a resync.
//!
//! Books stay in one flat vector indexed by [`ContractId`], which is what makes
//! a lookup a single index and keeps interning order — and therefore every
//! pinned digest — identical to the single-venue store this grew from. Only the
//! sequence expectation and the blast radius of an invalidation are per venue;
//! each [`Book`] already carries the venue it belongs to.
//!
//! `updated_at_ms` comes from the injected [`Clock`] (local time live, recorded
//! receipt time under replay), never from the venue timestamp on the event.

use crate::{
    clock::Clock,
    feed::FeedEvent,
    types::{Book, BookApplyError, BookState, ContractId, Contracts, VENUE_COUNT, Venue},
};
use std::sync::Arc;

/// Outcome of applying one [`FeedEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    Snapshot(ContractId),
    Delta(ContractId),
    /// Delta arrived while the book was not [`BookState::Live`].
    Skipped,
    /// Subscription sequence discontinuity; driver must request resync.
    Gap {
        expected: u64,
        got: u64,
    },
    UnknownContract,
    /// Disconnect or resubscribe invalidated books awaiting a fresh snapshot.
    Invalidated,
}

#[derive(Debug, Default, Clone)]
pub struct BookMetrics {
    pub snapshots_applied: u64,
    pub deltas_applied: u64,
    pub deltas_skipped_not_live: u64,
    pub sequence_gaps: u64,
    pub resync_requests: u64,
    pub negative_level_clamps: u64,
    pub crossed_books_observed: u64,
}

impl BookMetrics {
    pub fn books_by_state(store: &BookStore) -> (u64, u64, u64) {
        let mut uninitialized = 0u64;
        let mut resyncing = 0u64;
        let mut live = 0u64;
        for book in &store.books {
            match book.state {
                BookState::Uninitialized => uninitialized += 1,
                BookState::Resyncing => resyncing += 1,
                BookState::Live => live += 1,
            }
        }
        (uninitialized, resyncing, live)
    }

    pub fn log(&self, store: &BookStore) {
        let (uninitialized, resyncing, live) = Self::books_by_state(store);
        tracing::info!(?self, uninitialized, resyncing, live, "book store metrics");
    }
}

pub struct BookStore {
    books: Vec<Book>,
    contracts: Contracts,
    /// Next expected subscription sequence after the last accepted message, per
    /// venue. Indexed by [`Venue::index`].
    expected_seq: [Option<u64>; VENUE_COUNT],
    clock: Arc<dyn Clock>,
    pub metrics: BookMetrics,
}

impl BookStore {
    /// Intern tickers in the same order as [`crate::feed::kalshi::Parser::new`].
    pub fn new(venue: Venue, tickers: &[String], clock: Arc<dyn Clock>) -> anyhow::Result<Self> {
        Self::multi_venue(&[(venue, tickers)], clock)
    }

    /// Build a store spanning several venues.
    ///
    /// Subscriptions are interned in the order given, and the caller must pass
    /// them in the registry's order (every Kalshi member before any Polymarket
    /// one). Contract ids are positional, so a store interned in a different
    /// order from the registry that feeds it points every book at the wrong
    /// contract — which is why this takes an ordered slice rather than a map.
    pub fn multi_venue(
        subscriptions: &[(Venue, &[String])],
        clock: Arc<dyn Clock>,
    ) -> anyhow::Result<Self> {
        let mut contracts = Contracts::default();
        let mut books = Vec::new();
        for (venue, tickers) in subscriptions {
            for ticker in *tickers {
                let id = contracts.intern(*venue, ticker)?;
                books.push(Book::new(*venue, id));
            }
        }
        Ok(Self {
            books,
            contracts,
            expected_seq: [None; VENUE_COUNT],
            clock,
            metrics: BookMetrics::default(),
        })
    }

    /// Which venue an event belongs to.
    ///
    /// [`FeedEvent`] only names a venue on `Disconnected`; everything else
    /// carries a [`ContractId`], which is venue-scoped by construction. Reading
    /// it back off the book is therefore the authoritative answer, and it is
    /// `None` exactly when the contract is not one this store subscribes to.
    pub fn venue_of(&self, event: &FeedEvent) -> Option<Venue> {
        match event {
            FeedEvent::Disconnected { venue } => Some(*venue),
            FeedEvent::Snapshot { contract, .. }
            | FeedEvent::Delta { contract, .. }
            | FeedEvent::Resubscribed { contract } => self.get(*contract).map(|book| book.venue),
        }
    }

    /// Venues this store holds at least one book for.
    pub fn venues(&self) -> Vec<Venue> {
        Venue::ALL
            .into_iter()
            .filter(|venue| self.books.iter().any(|book| book.venue == *venue))
            .collect()
    }

    pub fn contracts(&self) -> &Contracts {
        &self.contracts
    }

    pub fn books(&self) -> &[Book] {
        &self.books
    }

    pub fn get(&self, id: ContractId) -> Option<&Book> {
        self.books.get(id.0 as usize)
    }

    pub fn expected_seq(&self, venue: Venue) -> Option<u64> {
        self.expected_seq[venue.index()]
    }

    pub fn note_resync_request(&mut self) {
        self.metrics.resync_requests = self.metrics.resync_requests.saturating_add(1);
    }

    /// Apply an event and report which contracts the solver must re-evaluate.
    ///
    /// The dirty set is a `Vec`, not a set or an iterator, so the engine walks
    /// it in a fixed order and replay produces the same evaluation sequence as
    /// the live run did.
    ///
    /// Nothing is marked dirty when the outcome left no book `Live`. A gap or a
    /// disconnect moves every book to `Resyncing`, and the solver refuses a
    /// group containing one, so handing those contracts over would be work whose
    /// only possible result is a `not_live` rejection per group.
    pub fn apply_and_mark(&mut self, event: &FeedEvent) -> (Applied, Vec<ContractId>) {
        let applied = self.apply(event);
        let dirty = match applied {
            Applied::Snapshot(contract) | Applied::Delta(contract) => vec![contract],
            Applied::Skipped
            | Applied::Gap { .. }
            | Applied::UnknownContract
            | Applied::Invalidated => Vec::new(),
        };
        (applied, dirty)
    }

    pub fn apply(&mut self, event: &FeedEvent) -> Applied {
        match event {
            FeedEvent::Snapshot {
                contract,
                yes,
                no,
                seq,
                venue_ts_ms: _,
            } => self.apply_snapshot(*contract, yes, no, *seq, self.clock.now_ms()),
            FeedEvent::Delta {
                contract,
                side,
                price,
                size_delta,
                seq,
                venue_ts_ms: _,
            } => self.apply_delta(
                *contract,
                *side,
                *price,
                *size_delta,
                *seq,
                self.clock.now_ms(),
            ),
            FeedEvent::Disconnected { venue } => {
                self.invalidate_venue(*venue);
                Applied::Invalidated
            }
            FeedEvent::Resubscribed { contract } => {
                let Some(venue) = self.get(*contract).map(|book| book.venue) else {
                    return Applied::UnknownContract;
                };
                // Subscription seq resets with a new handshake; clear expectation
                // so the next snapshot rebases without reporting a false gap.
                self.expected_seq[venue.index()] = None;
                self.mark_venue_resyncing(venue);
                Applied::Invalidated
            }
        }
    }

    fn apply_snapshot(
        &mut self,
        contract: ContractId,
        yes: &[crate::types::Level],
        no: &[crate::types::Level],
        seq: u64,
        ts_ms: u64,
    ) -> Applied {
        let Some(book) = self.books.get_mut(contract.0 as usize) else {
            return Applied::UnknownContract;
        };
        if book.contract_id != contract {
            return Applied::UnknownContract;
        }
        let venue = book.venue;
        if let Err(error) = book.apply_snapshot(yes, no, seq, ts_ms) {
            tracing::warn!(%error, ?contract, "snapshot rejected");
            return Applied::Skipped;
        }
        // Snapshot is absolute: always rebase this venue's expectation.
        self.expected_seq[venue.index()] = Some(seq.saturating_add(1));
        self.metrics.snapshots_applied = self.metrics.snapshots_applied.saturating_add(1);
        if book.is_crossed() {
            self.metrics.crossed_books_observed =
                self.metrics.crossed_books_observed.saturating_add(1);
        }
        Applied::Snapshot(contract)
    }

    fn apply_delta(
        &mut self,
        contract: ContractId,
        side: crate::types::Side,
        price: crate::types::Cents,
        size_delta: i64,
        seq: u64,
        ts_ms: u64,
    ) -> Applied {
        let Some(book) = self.books.get(contract.0 as usize) else {
            return Applied::UnknownContract;
        };
        if book.contract_id != contract {
            return Applied::UnknownContract;
        }
        if book.state != BookState::Live {
            self.metrics.deltas_skipped_not_live =
                self.metrics.deltas_skipped_not_live.saturating_add(1);
            return Applied::Skipped;
        }
        let venue = book.venue;

        match self.expected_seq[venue.index()] {
            Some(expected) if seq == expected => {}
            Some(expected) => {
                self.metrics.sequence_gaps = self.metrics.sequence_gaps.saturating_add(1);
                // Only this venue loses its books. The other venue's sequence
                // came from a different handshake and is still intact.
                self.mark_venue_resyncing(venue);
                self.expected_seq[venue.index()] = None;
                return Applied::Gap { expected, got: seq };
            }
            None => {
                // No snapshot yet on this subscription — treat as not live path.
                self.metrics.deltas_skipped_not_live =
                    self.metrics.deltas_skipped_not_live.saturating_add(1);
                return Applied::Skipped;
            }
        }

        let book = &mut self.books[contract.0 as usize];
        match book.apply_delta(side, price, size_delta, seq, ts_ms) {
            Ok(clamped) => {
                if clamped {
                    self.metrics.negative_level_clamps =
                        self.metrics.negative_level_clamps.saturating_add(1);
                }
                self.expected_seq[venue.index()] = Some(seq.saturating_add(1));
                self.metrics.deltas_applied = self.metrics.deltas_applied.saturating_add(1);
                if book.is_crossed() {
                    self.metrics.crossed_books_observed =
                        self.metrics.crossed_books_observed.saturating_add(1);
                }
                Applied::Delta(contract)
            }
            Err(BookApplyError::PriceOutOfRange(_)) => {
                tracing::warn!(?contract, price, "delta price out of range");
                Applied::Skipped
            }
            Err(error) => {
                tracing::warn!(%error, ?contract, "delta rejected");
                Applied::Skipped
            }
        }
    }

    /// Demote this venue's live books, leaving every other venue untouched.
    fn mark_venue_resyncing(&mut self, venue: Venue) {
        for book in &mut self.books {
            if book.venue == venue && book.state == BookState::Live {
                book.mark_resyncing();
            }
        }
    }

    /// A disconnect takes down every book behind that socket, live or not, and
    /// clears the expectation so the next snapshot rebases.
    fn invalidate_venue(&mut self, venue: Venue) {
        for book in &mut self.books {
            if book.venue == venue {
                book.mark_resyncing();
            }
        }
        self.expected_seq[venue.index()] = None;
    }
}

/// The solver reads books and engine time through the store.
///
/// Time comes from the same injected clock that stamps `updated_at_ms`, so the
/// freshness gate compares two readings of one clock. Reading wall time in the
/// solver instead would make replay disagree with the live run it replays.
impl crate::solver::BookSource for BookStore {
    fn book(&self, contract: ContractId) -> Option<&Book> {
        self.get(contract)
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }
}
