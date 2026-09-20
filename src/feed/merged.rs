//! One feed over several venues.
//!
//! The engine loop takes a single [`Feed`], and two venues have to reach it
//! without either one blocking the other. This selects across them and yields
//! whichever spoke first.
//!
//! # Arrival order, not timestamp order
//!
//! Events come out in the order the venues produce them. They are deliberately
//! *not* sorted by timestamp, because doing so would mean holding an event from
//! the fast venue until the slow one had been heard from, which converts a
//! latency advantage into a latency penalty and still could not produce a true
//! ordering — the next message from the quiet venue may be older than the one
//! already held. Nothing downstream needs a global order: sequence continuity
//! is scoped per venue in [`crate::book::BookStore`], and freshness is judged
//! per book against the clock.
//!
//! # Cancellation
//!
//! `select!` drops the losing future on every pass, so each feed's `next` is
//! polled and abandoned repeatedly. That is safe only because [`Feed`] requires
//! cancel-safety, and it is the reason that requirement is written down.

use crate::{
    feed::{Feed, FeedEvent},
    metrics::Metrics,
};
use std::{future::Future, pin::Pin};

/// Drives several feeds as one.
pub struct MergedFeed {
    feeds: Vec<Box<dyn Feed>>,
    /// Feeds that have returned `None`. A finished feed is never polled again:
    /// its future would complete immediately every pass and starve the others.
    finished: Vec<bool>,
}

impl MergedFeed {
    pub fn new(feeds: Vec<Box<dyn Feed>>) -> Self {
        let finished = vec![false; feeds.len()];
        MergedFeed { feeds, finished }
    }

    /// Whether every feed has finished.
    pub fn is_finished(&self) -> bool {
        self.finished.iter().all(|done| *done)
    }

    async fn next_event(&mut self) -> Option<FeedEvent> {
        loop {
            // Poll only the live feeds, and only for as long as it takes one to
            // speak. `select_all` needs a non-empty set, so exhaustion is
            // checked first rather than discovered inside it.
            let mut pending = Vec::new();
            let mut index_of = Vec::new();
            for (index, feed) in self.feeds.iter_mut().enumerate() {
                if self.finished[index] {
                    continue;
                }
                pending.push(feed.next());
                index_of.push(index);
            }
            if pending.is_empty() {
                return None;
            }

            let (event, which, _rest) = futures_util::future::select_all(pending).await;
            match event {
                Some(event) => return Some(event),
                // That feed is done; the others may still have more to say.
                None => self.finished[index_of[which]] = true,
            }
        }
    }
}

impl Feed for MergedFeed {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>> {
        Box::pin(self.next_event())
    }

    /// Ask every venue for a fresh snapshot.
    ///
    /// Coarser than it could be: the engine reports a gap without naming the
    /// venue it happened on, so the only safe reading is that some venue needs
    /// resynchronising. Resyncing a healthy venue costs a snapshot; missing the
    /// one that gapped costs a wrong book.
    fn request_resync(&self) {
        for feed in &self.feeds {
            feed.request_resync();
        }
    }

    /// The venues' counters, summed.
    ///
    /// A single total is what the trait can carry, and it is enough for the
    /// aggregate the dashboard shows beside the per-venue panel. Attributing
    /// each venue's own counters to it needs a per-venue accessor, which is why
    /// the dashboard currently shows every subscribed venue the same figures.
    fn metrics(&self) -> Option<Metrics> {
        let mut total: Option<Metrics> = None;
        for feed in &self.feeds {
            let Some(metrics) = feed.metrics() else {
                continue;
            };
            let sum = total.get_or_insert_with(Metrics::default);
            sum.messages_received += metrics.messages_received;
            sum.parse_errors += metrics.parse_errors;
            sum.parse_attempts += metrics.parse_attempts;
            sum.unknown_messages += metrics.unknown_messages;
            sum.reconnections += metrics.reconnections;
            sum.bytes_recorded += metrics.bytes_recorded;
            sum.discarded_size_hundredths += metrics.discarded_size_hundredths;
            sum.snapshot_sides_absent += metrics.snapshot_sides_absent;
            sum.clock_skew_samples += metrics.clock_skew_samples;
            for (slot, count) in sum
                .latency_buckets
                .iter_mut()
                .zip(metrics.latency_buckets.iter())
            {
                *slot += count;
            }
        }
        total
    }
}
