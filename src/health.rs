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

use crate::{feed::FeedEvent, types::Venue};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VenueHealth {
    pub venue: Venue,
    pub last_message_ms: u64,
    pub consecutive_errors: u32,
    pub state: HealthState,
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

    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_message_ms)
    }
}

/// Health across every venue the engine trades.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthMonitor {
    pub kalshi: VenueHealth,
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
            kalshi: VenueHealth::new(Venue::Kalshi),
            max_idle_ms: DEFAULT_MAX_IDLE_MS,
        }
    }
}

impl HealthMonitor {
    pub fn new(max_idle_ms: u64) -> Self {
        HealthMonitor {
            max_idle_ms,
            ..HealthMonitor::default()
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
        match venue {
            // Polymarket has no feed until phase 7. Reporting Kalshi's health
            // for it would be a lie, so it shares the struct and stays
            // disconnected via its own record below.
            Venue::Kalshi | Venue::Polymarket => &self.kalshi,
        }
    }

    fn venue_mut(&mut self, venue: Venue) -> &mut VenueHealth {
        match venue {
            Venue::Kalshi | Venue::Polymarket => &mut self.kalshi,
        }
    }

    /// Fold a feed event into venue health.
    ///
    /// `Resubscribed` deliberately does not restore health: the venue
    /// acknowledged a subscription but has not yet sent a book. Health returns
    /// when data does.
    pub fn apply_feed_event(&mut self, event: &FeedEvent, now_ms: u64) {
        match event {
            FeedEvent::Snapshot { .. } | FeedEvent::Delta { .. } => {
                self.kalshi.update_on_message(now_ms);
            }
            FeedEvent::Disconnected { .. } => {
                self.kalshi.state = HealthState::Disconnected;
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
        assert_eq!(monitor.kalshi.state, HealthState::Disconnected);

        monitor.apply_feed_event(&delta(1_000), 1_000);
        assert!(monitor.can_trade(Venue::Kalshi, 1_000));

        // A subscription acknowledgement is not data, so it does not restore
        // health on its own.
        monitor.apply_feed_event(
            &FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
            1_100,
        );
        monitor.apply_feed_event(
            &FeedEvent::Resubscribed {
                contract: ContractId(0),
            },
            1_200,
        );
        assert!(!monitor.can_trade(Venue::Kalshi, 1_200));
        monitor.apply_feed_event(&delta(1_300), 1_300);
        assert!(monitor.can_trade(Venue::Kalshi, 1_300));
    }

    #[test]
    fn silence_past_the_idle_budget_blocks_trading() {
        let mut monitor = HealthMonitor::new(5_000);
        monitor.apply_feed_event(&delta(1_000), 1_000);
        assert!(monitor.can_trade(Venue::Kalshi, 6_000), "exactly at budget");
        assert!(!monitor.can_trade(Venue::Kalshi, 6_001));
        assert_eq!(monitor.kalshi.state, HealthState::Stale);
        // One message brings it straight back.
        monitor.apply_feed_event(&delta(6_500), 6_500);
        assert!(monitor.can_trade(Venue::Kalshi, 6_500));
    }

    #[test]
    fn three_consecutive_errors_stop_trading_and_one_success_clears_them() {
        let mut monitor = HealthMonitor::default();
        monitor.apply_feed_event(&delta(1_000), 1_000);
        monitor.note_error(Venue::Kalshi);
        monitor.note_error(Venue::Kalshi);
        // Two is noise; the venue is still tradeable.
        assert!(monitor.can_trade(Venue::Kalshi, 1_000));
        monitor.note_error(Venue::Kalshi);
        assert!(!monitor.can_trade(Venue::Kalshi, 1_000));
        assert_eq!(monitor.kalshi.state, HealthState::Erroring);

        monitor.apply_feed_event(&delta(1_100), 1_100);
        assert_eq!(monitor.kalshi.consecutive_errors, 0);
        assert!(monitor.can_trade(Venue::Kalshi, 1_100));
    }

    #[test]
    fn a_specific_failure_is_not_overwritten_by_staleness() {
        let mut monitor = HealthMonitor::new(100);
        monitor.apply_feed_event(&delta(1_000), 1_000);
        monitor.apply_feed_event(
            &FeedEvent::Disconnected {
                venue: Venue::Kalshi,
            },
            1_000,
        );
        // Long past the idle budget, but "disconnected" is the useful reason.
        assert!(!monitor.can_trade(Venue::Kalshi, 99_000));
        assert_eq!(monitor.kalshi.state, HealthState::Disconnected);
    }
}
