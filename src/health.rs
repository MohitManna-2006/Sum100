//! Venue health: is this feed telling us the truth right now?
//!
//! The solver's freshness gate already refuses a stale *book*. This is the
//! coarser question one level up: is the venue connection itself working. A book
//! can look perfectly fresh while the socket behind it has been silently dead
//! for a minute, because nothing arrived to make it stale — an idle far strike
//! and a dead feed produce identical books.
//!
//! Health blocks trading, never book keeping. An unhealthy venue still applies
//! every event it delivers; it just stops being allowed to spend money.

use crate::{
    feed::FeedEvent,
    types::{VENUE_COUNT, Venue},
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Healthy,
    /// Nothing has arrived for longer than the idle budget.
    Stale,
    /// Consecutive failures against the venue's API.
    Erroring,
    /// The feed reported the socket gone.
    Disconnected,
}

/// Errors before a venue is considered broken.
///
/// One failure is noise: a timeout, a 503, a dropped packet. Three in a row with
/// no success between them is a pattern, and continuing to send orders into it
/// is how you discover an outage by losing money in it.
pub const ERROR_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct VenueHealth {
    pub venue: Venue,
    pub last_message_ms: u64,
    pub consecutive_errors: u32,
    pub state: HealthState,
    /// Whether anything feeds this venue on this run.
    ///
    /// An unsubscribed venue is absent, not broken. The distinction matters
    /// because it is otherwise indistinguishable from a venue whose socket
    /// never came up, and rolling it up as a failure would report a healthy
    /// single-venue run as permanently degraded.
    pub subscribed: bool,
}

impl VenueHealth {
    /// Starts [`HealthState::Disconnected`]: a venue is not healthy because
    /// nobody has said otherwise. Health has to be earned by a message.
    pub fn new(venue: Venue) -> Self {
        VenueHealth {
            venue,
            last_message_ms: 0,
            consecutive_errors: 0,
            state: HealthState::Disconnected,
            subscribed: false,
        }
    }

    /// A message arrived: the socket is alive and the error streak is broken.
    pub fn update_on_message(&mut self, now_ms: u64) {
        self.last_message_ms = now_ms;
        self.consecutive_errors = 0;
        self.state = HealthState::Healthy;
    }

    pub fn update_on_error(&mut self) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        if self.consecutive_errors >= ERROR_THRESHOLD {
            self.state = HealthState::Erroring;
        }
    }

    /// Health as of `now_ms`, applying the idle budget without recording it.
    ///
    /// The read-only twin of [`VenueHealth::check_staleness`], for the publish
    /// path: a dashboard asking how things look must not be what demotes a
    /// venue, or the answer would depend on whether anyone was watching.
    pub fn is_healthy_at(&self, now_ms: u64, max_idle_ms: u64) -> bool {
        self.state == HealthState::Healthy
            && now_ms.saturating_sub(self.last_message_ms) <= max_idle_ms
    }

    /// Demote to [`HealthState::Stale`] once the idle budget is exceeded.
    ///
    /// Only ever demotes from `Healthy`. A venue that is `Erroring` or
    /// `Disconnected` has a more specific problem, and overwriting it with
    /// "stale" would lose the reason.
    pub fn check_staleness(&mut self, now_ms: u64, max_idle_ms: u64) {
        if self.state == HealthState::Healthy
            && now_ms.saturating_sub(self.last_message_ms) > max_idle_ms
        {
            self.state = HealthState::Stale;
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.state == HealthState::Healthy
    }

    /// Time since the last message, or zero if there has never been one.
    ///
    /// A venue that has never spoken has no idle time to report. Subtracting
    /// from an unset timestamp would give time since the epoch, which renders as
    /// a feed that has been silent for decades rather than one that was never
    /// subscribed — the state beside this is what says which.
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        if self.last_message_ms == 0 {
            return 0;
        }
        now_ms.saturating_sub(self.last_message_ms)
    }
}

/// How the engine as a whole is doing, rolled up over subscribed venues.
///
/// Reported beside the per-venue detail rather than instead of it: an operator
/// needs one field to glance at, and the venue that caused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverallHealth {
    /// Every subscribed venue is healthy.
    Connected,
    /// Some subscribed venue is healthy and some is not. Trading continues on
    /// the venues that are up; anything needing the others is refused.
    Degraded,
    /// No subscribed venue is healthy, so nothing can be traded. An engine with
    /// no subscriptions at all reports this too, since the question this answers
    /// is whether trading is possible right now, and it is not.
    Erroring,
}

/// Health across every venue the engine trades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthMonitor {
    venues: [VenueHealth; VENUE_COUNT],
    pub max_idle_ms: u64,
}

/// Default idle budget: ten times the book freshness budget.
///
/// These measure different things. `max_book_age_ms` asks whether one quote is
/// current; this asks whether the connection is alive at all, and a quiet
/// market can legitimately go seconds without a single update across every
/// subscribed contract.
pub const DEFAULT_MAX_IDLE_MS: u64 = 5_000;

impl Default for HealthMonitor {
    fn default() -> Self {
        HealthMonitor {
            venues: Venue::ALL.map(VenueHealth::new),
            max_idle_ms: DEFAULT_MAX_IDLE_MS,
        }
    }
}

impl HealthMonitor {
    /// Every venue tracked, none subscribed. Prefer [`HealthMonitor::for_venues`]
    /// on a real run so the rollup knows what it is allowed to judge.
    pub fn new(max_idle_ms: u64) -> Self {
        HealthMonitor {
            max_idle_ms,
            ..HealthMonitor::default()
        }
    }

    /// Track every venue, and mark these as ones the rollup may judge.
    pub fn for_venues(max_idle_ms: u64, subscribed: &[Venue]) -> Self {
        let mut monitor = HealthMonitor::new(max_idle_ms);
        for venue in subscribed {
            monitor.venues[venue.index()].subscribed = true;
        }
        monitor
    }

    pub fn all(&self) -> &[VenueHealth] {
        &self.venues
    }

    /// Roll the subscribed venues up into one verdict.
    ///
    /// Read-only, unlike [`HealthMonitor::can_trade`]: publishing state must not
    /// change it, so staleness is evaluated here rather than recorded.
    pub fn overall(&self, now_ms: u64) -> OverallHealth {
        let budget = max_idle(self.max_idle_ms);
        let mut healthy = 0usize;
        let mut subscribed = 0usize;
        for health in &self.venues {
            if !health.subscribed {
                continue;
            }
            subscribed += 1;
            if health.is_healthy_at(now_ms, budget) {
                healthy += 1;
            }
        }
        match healthy {
            0 => OverallHealth::Erroring,
            n if n == subscribed => OverallHealth::Connected,
            _ => OverallHealth::Degraded,
        }
    }

    /// Whether this venue may be traded right now. Evaluates staleness first,
    /// so a caller cannot read a `Healthy` that has quietly expired.
    pub fn can_trade(&mut self, venue: Venue, now_ms: u64) -> bool {
        let budget = max_idle(self.max_idle_ms);
        let health = self.venue_mut(venue);
        health.check_staleness(now_ms, budget);
        health.is_healthy()
    }

    pub fn venue(&self, venue: Venue) -> &VenueHealth {
        &self.venues[venue.index()]
    }

    fn venue_mut(&mut self, venue: Venue) -> &mut VenueHealth {
        &mut self.venues[venue.index()]
    }

    /// Fold a feed event into one venue's health.
    ///
    /// The venue is passed in rather than read off the event, because only
    /// `Disconnected` names one; the rest carry a [`crate::types::ContractId`],
    /// and the book store owns the mapping from that to a venue.
    ///
    /// `Resubscribed` deliberately does not restore health: the venue
    /// acknowledged a subscription but has not yet sent a book. Health returns
    /// when data does.
    pub fn apply_feed_event(&mut self, event: &FeedEvent, venue: Venue, now_ms: u64) {
        match event {
            FeedEvent::Snapshot { .. } | FeedEvent::Delta { .. } | FeedEvent::LevelSet { .. } => {
                self.venue_mut(venue).update_on_message(now_ms);
            }
            FeedEvent::Disconnected { .. } => {
                self.venue_mut(venue).state = HealthState::Disconnected;
            }
            FeedEvent::Resubscribed { .. } => {}
        }
    }

    pub fn note_error(&mut self, venue: Venue) {
        self.venue_mut(venue).update_on_error();
    }
}

/// A zero budget would mark every venue stale the instant it went healthy,
/// which reads as a misconfiguration rather than an intent to never trade.
fn max_idle(configured: u64) -> u64 {
    configured.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContractId, Side};

    fn delta(now: u64) -> FeedEvent {
        FeedEvent::Delta {
            contract: ContractId(0),
            side: Side::Yes,
            price: 40,
            size_delta: 1,
            seq: 1,
            venue_ts_ms: now,
        }
    }

    #[test]
    fn a_venue_starts_untrusted_and_earns_health_from_data() {
        let mut monitor = HealthMonitor::default();
        assert!(!monitor.can_trade(Venue::Kalshi, 1_000));
        assert_eq!(
            monitor.venue(Venue::Kalshi).state,
            HealthState::Disconnected
        );

        monitor.apply_feed_event(&delta(1_000), Venue::Kalshi, 1_000);
        assert!(monitor.can_trade(Venue::Kalshi, 1_000));

        // A subscription acknowledgement is not data, so it does not restore
        // health on its own.
        monitor.apply_feed_event(
            &FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
            Venue::Kalshi,
            1_100,
        );
        monitor.apply_feed_event(
            &FeedEvent::Resubscribed {
                contract: ContractId(0),
            },
            Venue::Kalshi,
            1_200,
        );
        assert!(!monitor.can_trade(Venue::Kalshi, 1_200));
        monitor.apply_feed_event(&delta(1_300), Venue::Kalshi, 1_300);
        assert!(monitor.can_trade(Venue::Kalshi, 1_300));
    }

    #[test]
    fn silence_past_the_idle_budget_blocks_trading() {
        let mut monitor = HealthMonitor::new(5_000);
        monitor.apply_feed_event(&delta(1_000), Venue::Kalshi, 1_000);
        assert!(monitor.can_trade(Venue::Kalshi, 6_000), "exactly at budget");
        assert!(!monitor.can_trade(Venue::Kalshi, 6_001));
        assert_eq!(monitor.venue(Venue::Kalshi).state, HealthState::Stale);
        // One message brings it straight back.
        monitor.apply_feed_event(&delta(6_500), Venue::Kalshi, 6_500);
        assert!(monitor.can_trade(Venue::Kalshi, 6_500));
    }

    #[test]
    fn three_consecutive_errors_stop_trading_and_one_success_clears_them() {
        let mut monitor = HealthMonitor::default();
        monitor.apply_feed_event(&delta(1_000), Venue::Kalshi, 1_000);
        monitor.note_error(Venue::Kalshi);
        monitor.note_error(Venue::Kalshi);
        // Two is noise; the venue is still tradeable.
        assert!(monitor.can_trade(Venue::Kalshi, 1_000));
        monitor.note_error(Venue::Kalshi);
        assert!(!monitor.can_trade(Venue::Kalshi, 1_000));
        assert_eq!(monitor.venue(Venue::Kalshi).state, HealthState::Erroring);

        monitor.apply_feed_event(&delta(1_100), Venue::Kalshi, 1_100);
        assert_eq!(monitor.venue(Venue::Kalshi).consecutive_errors, 0);
        assert!(monitor.can_trade(Venue::Kalshi, 1_100));
    }

    #[test]
    fn a_specific_failure_is_not_overwritten_by_staleness() {
        let mut monitor = HealthMonitor::new(100);
        monitor.apply_feed_event(&delta(1_000), Venue::Kalshi, 1_000);
        monitor.apply_feed_event(
            &FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
            Venue::Kalshi,
            1_000,
        );
        // Long past the idle budget, but "disconnected" is the useful reason.
        assert!(!monitor.can_trade(Venue::Kalshi, 99_000));
        assert_eq!(
            monitor.venue(Venue::Kalshi).state,
            HealthState::Disconnected
        );
    }

    /// Two venues, two sockets, two independent verdicts. One going down must
    /// not cost the other its health, or a Polymarket outage would stop Kalshi
    /// trading for no reason.
    #[test]
    fn one_venue_failing_leaves_the_other_tradeable() {
        let mut monitor = HealthMonitor::for_venues(5_000, &[Venue::Kalshi, Venue::Polymarket]);
        monitor.apply_feed_event(&delta(1_000), Venue::Kalshi, 1_000);
        monitor.apply_feed_event(&delta(1_000), Venue::Polymarket, 1_000);
        assert_eq!(monitor.overall(1_000), OverallHealth::Connected);

        monitor.apply_feed_event(
            &FeedEvent::Disconnected {
                venue: Venue::Polymarket,
            },
            Venue::Polymarket,
            1_100,
        );
        assert!(monitor.can_trade(Venue::Kalshi, 1_100));
        assert!(!monitor.can_trade(Venue::Polymarket, 1_100));
        assert_eq!(monitor.overall(1_100), OverallHealth::Degraded);

        // Errors are counted against the venue that produced them.
        monitor.note_error(Venue::Polymarket);
        assert_eq!(monitor.venue(Venue::Kalshi).consecutive_errors, 0);
        assert_eq!(monitor.venue(Venue::Polymarket).consecutive_errors, 1);
    }

    /// A venue nobody subscribed to is absent, not broken. Counting it as a
    /// failure would hold every single-venue run at Degraded forever.
    #[test]
    fn an_unsubscribed_venue_is_absent_rather_than_unhealthy() {
        let mut monitor = HealthMonitor::for_venues(5_000, &[Venue::Kalshi]);
        monitor.apply_feed_event(&delta(1_000), Venue::Kalshi, 1_000);

        assert_eq!(monitor.overall(1_000), OverallHealth::Connected);
        assert!(!monitor.venue(Venue::Polymarket).subscribed);
        assert!(monitor.venue(Venue::Kalshi).subscribed);

        // And once the only subscribed venue goes, there is nothing left to
        // trade on, which is Erroring rather than Degraded.
        monitor.apply_feed_event(
            &FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
            Venue::Kalshi,
            1_100,
        );
        assert_eq!(monitor.overall(1_100), OverallHealth::Erroring);
    }
}
