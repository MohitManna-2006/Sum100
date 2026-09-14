//! Book store: apply feed events to in-memory books with sequence continuity.
//!
//! Sequence numbers are scoped to the venue subscription, not per contract. A
//! gap on any ticker invalidates every live book on that subscription. The
//! store never calls back into the feed; on [`Applied::Gap`] the driver must
//! request a resync.

use crate::{
    feed::FeedEvent,
    types::{Book, BookApplyError, BookState, ContractId, Contracts, Venue},
};

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
    /// Next expected subscription sequence after the last accepted message.
    expected_seq: Option<u64>,
    pub metrics: BookMetrics,
}

impl BookStore {
    /// Intern tickers in the same order as [`crate::feed::kalshi::Parser::new`].
    pub fn new(venue: Venue, tickers: &[String]) -> anyhow::Result<Self> {
        let mut contracts = Contracts::default();
        let mut books = Vec::with_capacity(tickers.len());
        for ticker in tickers {
            let id = contracts.intern(venue, ticker)?;
            books.push(Book::new(venue, id));
        }
        Ok(Self {
            books,
            contracts,
            expected_seq: None,
            metrics: BookMetrics::default(),
        })
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

    pub fn expected_seq(&self) -> Option<u64> {
        self.expected_seq
    }

    pub fn note_resync_request(&mut self) {
        self.metrics.resync_requests = self.metrics.resync_requests.saturating_add(1);
    }

    pub fn apply(&mut self, event: &FeedEvent) -> Applied {
        match event {
            FeedEvent::Snapshot {
                contract,
                yes,
                no,
                seq,
                ts_ms,
            } => self.apply_snapshot(*contract, yes, no, *seq, *ts_ms),
            FeedEvent::Delta {
                contract,
                side,
                price,
                size_delta,
                seq,
                ts_ms,
            } => self.apply_delta(*contract, *side, *price, *size_delta, *seq, *ts_ms),
            FeedEvent::Disconnected { .. } => {
                self.invalidate_all();
                Applied::Invalidated
            }
            FeedEvent::Resubscribed { .. } => {
                // Subscription seq resets with a new handshake; clear expectation
                // so the next snapshot rebases without reporting a false gap.
                self.expected_seq = None;
                for book in &mut self.books {
                    if book.state == BookState::Live {
                        book.mark_resyncing();
                    }
                }
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
        if let Err(error) = book.apply_snapshot(yes, no, seq, ts_ms) {
            tracing::warn!(%error, ?contract, "snapshot rejected");
            return Applied::Skipped;
        }
        // Snapshot is absolute: always rebase the subscription expectation.
        self.expected_seq = Some(seq.saturating_add(1));
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

        match self.expected_seq {
            Some(expected) if seq == expected => {}
            Some(expected) => {
                self.metrics.sequence_gaps = self.metrics.sequence_gaps.saturating_add(1);
                self.mark_all_resyncing();
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
                self.expected_seq = Some(seq.saturating_add(1));
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

    fn mark_all_resyncing(&mut self) {
        for book in &mut self.books {
            if book.state == BookState::Live {
                book.mark_resyncing();
            }
        }
        // After a gap, wait for a snapshot to rebase; do not accept deltas.
        self.expected_seq = None;
    }

    fn invalidate_all(&mut self) {
        for book in &mut self.books {
            book.mark_resyncing();
        }
        self.expected_seq = None;
    }
}
