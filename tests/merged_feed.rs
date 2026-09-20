//! Merging two venues into the one feed the engine loop takes.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use sum100::{
    feed::{Feed, FeedEvent, merged::MergedFeed},
    metrics::Metrics,
    types::{ContractId, Side, TokenSide, Venue},
};

/// Yields prepared events, optionally pausing before each one.
///
/// The pause is what makes this a real test of merging: with both feeds ready
/// instantly the runtime picks between them arbitrarily, and nothing about
/// interleaving is being exercised.
struct ScriptedFeed {
    events: Vec<FeedEvent>,
    index: usize,
    delay: Duration,
    polls: Arc<AtomicUsize>,
}

impl ScriptedFeed {
    fn new(events: Vec<FeedEvent>, delay_ms: u64) -> Self {
        ScriptedFeed {
            events,
            index: 0,
            delay: Duration::from_millis(delay_ms),
            polls: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn yield_next(&mut self) -> Option<FeedEvent> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        if self.index >= self.events.len() {
            return None;
        }
        // The await lands before the state moves, so a dropped future leaves
        // the feed exactly where it was. That is the contract `Feed` states.
        tokio::time::sleep(self.delay).await;
        let event = self.events[self.index].clone();
        self.index += 1;
        Some(event)
    }
}

impl Feed for ScriptedFeed {
    fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>> {
        Box::pin(self.yield_next())
    }

    fn metrics(&self) -> Option<Metrics> {
        Some(Metrics {
            messages_received: self.events.len() as u64,
            parse_errors: 1,
            ..Metrics::default()
        })
    }
}

fn delta(contract: u32, seq: u64) -> FeedEvent {
    FeedEvent::Delta {
        contract: ContractId(contract),
        side: Side::Yes,
        price: 40,
        size_delta: 1,
        seq,
        venue_ts_ms: 1_000 + seq,
    }
}

fn level_set(contract: u32, price: i64) -> FeedEvent {
    FeedEvent::LevelSet {
        contract: ContractId(contract),
        side: TokenSide::Bid,
        price,
        size: 100,
        venue_ts_ms: 2_000,
    }
}

async fn drain(feed: &mut MergedFeed) -> Vec<FeedEvent> {
    let mut seen = Vec::new();
    while let Some(event) = feed.next().await {
        seen.push(event);
    }
    seen
}

/// Nothing is dropped and nothing is duplicated, whichever venue is faster.
#[tokio::test]
async fn every_event_from_every_feed_arrives_exactly_once() {
    let kalshi = ScriptedFeed::new(vec![delta(0, 1), delta(0, 2), delta(0, 3)], 1);
    let polymarket = ScriptedFeed::new(vec![level_set(1, 40), level_set(1, 41)], 3);
    let mut merged = MergedFeed::new(vec![Box::new(kalshi), Box::new(polymarket)]);

    let seen = drain(&mut merged).await;
    assert_eq!(seen.len(), 5);
    assert!(merged.is_finished());

    let deltas: Vec<u64> = seen
        .iter()
        .filter_map(|event| match event {
            FeedEvent::Delta { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect();
    let levels: Vec<i64> = seen
        .iter()
        .filter_map(|event| match event {
            FeedEvent::LevelSet { price, .. } => Some(*price),
            _ => None,
        })
        .collect();

    // Each venue's own order survives, which is what sequence continuity in the
    // book store depends on. The order *between* venues is arrival order and is
    // deliberately not asserted.
    assert_eq!(deltas, vec![1, 2, 3]);
    assert_eq!(levels, vec![40, 41]);
}

/// A feed that ends must not end the merge, and must not be polled again —
/// a finished future completes instantly and would starve the other venue.
#[tokio::test]
async fn one_venue_finishing_does_not_stop_the_other() {
    let kalshi = ScriptedFeed::new(vec![delta(0, 1)], 1);
    let polls = Arc::new(AtomicUsize::new(0));
    let mut polymarket = ScriptedFeed::new(vec![level_set(1, 40), level_set(1, 41)], 2);
    polymarket.polls = Arc::clone(&polls);

    let mut merged = MergedFeed::new(vec![Box::new(kalshi), Box::new(polymarket)]);
    let seen = drain(&mut merged).await;

    assert_eq!(seen.len(), 3, "one Kalshi event and both Polymarket ones");
    assert!(merged.is_finished());

    // A finished feed is never polled again. Its future would complete at once
    // every pass, spinning the merge and starving anything still live.
    let settled = polls.load(Ordering::Relaxed);
    assert!(merged.next().await.is_none());
    assert!(merged.next().await.is_none());
    assert_eq!(polls.load(Ordering::Relaxed), settled);
}

/// The merge polls a feed and abandons it whenever the other venue wins the
/// race, which is exactly why `Feed` requires cancel-safety. Pinning it here
/// means a future implementation that buffers across an await has a test
/// telling it so.
#[tokio::test]
async fn a_losing_feed_is_polled_and_dropped_without_losing_its_place() {
    let polls = Arc::new(AtomicUsize::new(0));
    let mut slow = ScriptedFeed::new(vec![level_set(1, 40)], 20);
    slow.polls = Arc::clone(&polls);
    // Several quick events, so the slow feed loses the race repeatedly.
    let fast = ScriptedFeed::new((1..=4).map(|seq| delta(0, seq)).collect(), 1);

    let mut merged = MergedFeed::new(vec![Box::new(fast), Box::new(slow)]);
    let seen = drain(&mut merged).await;

    // Cancelled polls outnumber the events the slow feed ever yielded, and it
    // still delivered every one of them.
    assert!(
        polls.load(Ordering::Relaxed) > 2,
        "expected the slow feed to be cancelled repeatedly"
    );
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, FeedEvent::LevelSet { .. }))
            .count(),
        1
    );
    assert_eq!(seen.len(), 5);
}

/// A merge of nothing is finished, not hung.
#[tokio::test]
async fn an_empty_merge_finishes_immediately() {
    let mut merged = MergedFeed::new(Vec::new());
    assert!(merged.is_finished());
    assert!(merged.next().await.is_none());
}

/// Both venues' counters reach the dashboard, as one aggregate.
#[tokio::test]
async fn metrics_sum_across_venues() {
    let kalshi = ScriptedFeed::new(vec![delta(0, 1), delta(0, 2)], 0);
    let polymarket = ScriptedFeed::new(vec![level_set(1, 40)], 0);
    let merged = MergedFeed::new(vec![Box::new(kalshi), Box::new(polymarket)]);

    let metrics = merged.metrics().expect("both feeds keep counters");
    assert_eq!(metrics.messages_received, 3);
    assert_eq!(metrics.parse_errors, 2, "one from each venue");
}

/// The slow venue must not hold up the fast one. With a ten-to-one difference
/// the fast feed should drain well before the slow one is done.
#[tokio::test]
async fn a_slow_venue_does_not_block_a_fast_one() {
    let fast: Vec<FeedEvent> = (1..=6).map(|seq| delta(0, seq)).collect();
    let kalshi = ScriptedFeed::new(fast, 1);
    let polymarket = ScriptedFeed::new(vec![level_set(1, 40), level_set(1, 41)], 25);
    let mut merged = MergedFeed::new(vec![Box::new(kalshi), Box::new(polymarket)]);

    let mut fast_seen = 0usize;
    let mut fast_done_before_first_slow = 0usize;
    while let Some(event) = merged.next().await {
        match event {
            FeedEvent::Delta { .. } => fast_seen += 1,
            FeedEvent::LevelSet { .. } if fast_done_before_first_slow == 0 => {
                fast_done_before_first_slow = fast_seen;
            }
            _ => {}
        }
    }
    assert_eq!(fast_seen, 6);
    assert!(
        fast_done_before_first_slow >= 5,
        "fast venue delivered only {fast_done_before_first_slow} before the slow one"
    );
}

/// A resync is a fact about one subscription. Dropping the other venue's socket
/// to repair it would discard live books to fix a venue that was never wrong.
#[tokio::test]
async fn a_resync_reaches_only_the_venue_it_names() {
    struct Counting {
        venue: Venue,
        resyncs: Arc<AtomicUsize>,
    }

    impl Feed for Counting {
        fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn venue(&self) -> Option<Venue> {
            Some(self.venue)
        }

        fn request_resync(&self, venue: Venue) {
            if venue == self.venue {
                self.resyncs.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let kalshi = Arc::new(AtomicUsize::new(0));
    let polymarket = Arc::new(AtomicUsize::new(0));
    let merged = MergedFeed::new(vec![
        Box::new(Counting {
            venue: Venue::Kalshi,
            resyncs: Arc::clone(&kalshi),
        }),
        Box::new(Counting {
            venue: Venue::Polymarket,
            resyncs: Arc::clone(&polymarket),
        }),
    ]);

    merged.request_resync(Venue::Kalshi);
    assert_eq!(kalshi.load(Ordering::Relaxed), 1);
    assert_eq!(polymarket.load(Ordering::Relaxed), 0, "left alone");

    merged.request_resync(Venue::Polymarket);
    assert_eq!(kalshi.load(Ordering::Relaxed), 1);
    assert_eq!(polymarket.load(Ordering::Relaxed), 1);
}

/// Counters stay attributed to the venue that produced them. Reporting one
/// total would make a dead feed beside a busy one look healthy.
#[tokio::test]
async fn counters_stay_attributed_to_their_own_venue() {
    struct Venued {
        venue: Venue,
        messages: u64,
    }

    impl Feed for Venued {
        fn next(&mut self) -> Pin<Box<dyn Future<Output = Option<FeedEvent>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn venue(&self) -> Option<Venue> {
            Some(self.venue)
        }

        fn metrics(&self) -> Option<Metrics> {
            Some(Metrics {
                messages_received: self.messages,
                ..Metrics::default()
            })
        }
    }

    let merged = MergedFeed::new(vec![
        Box::new(Venued {
            venue: Venue::Kalshi,
            messages: 900,
        }),
        Box::new(Venued {
            venue: Venue::Polymarket,
            messages: 3,
        }),
    ]);

    let by_venue = merged.metrics_by_venue();
    assert_eq!(by_venue.len(), 2);
    let kalshi = by_venue
        .iter()
        .find(|(venue, _)| *venue == Venue::Kalshi)
        .unwrap();
    let polymarket = by_venue
        .iter()
        .find(|(venue, _)| *venue == Venue::Polymarket)
        .unwrap();
    assert_eq!(kalshi.1.messages_received, 900);
    assert_eq!(polymarket.1.messages_received, 3);

    // The aggregate is still available for the summary line beside the panel.
    assert_eq!(merged.metrics().unwrap().messages_received, 903);
}
