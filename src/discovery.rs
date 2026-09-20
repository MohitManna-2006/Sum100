//! Build the constraint graph from the venue instead of from a file.
//!
//! # What the venue actually tells us
//!
//! Kalshi does not list a "yes market" and a "no market". Every market is one
//! binary contract carrying both outcomes, so the complement relation is a
//! property of a single ticker, not a pair of them. The two structures worth
//! inferring come from metadata the venue publishes directly:
//!
//! - `event.mutually_exclusive` is the venue's own word that exactly one market
//!   under an event resolves yes. That is an exhaustive set, stated rather than
//!   guessed.
//! - `market.strike_type == "greater"` with a `floor_strike` is a threshold
//!   rung. A run of them under one non-exclusive event is a ladder, ordered by
//!   ascending threshold: "above 71,600" is at least as likely as "above
//!   71,700".
//!
//! Nothing else is inferred. Events whose markets carry `custom` strikes are
//! left alone, because the only honest reading of a custom strike is that the
//! venue declined to describe the structure and a guess would be a guess.
//!
//! # Why inferred groups are marked
//!
//! An inferred relation that is wrong does not produce silence, it produces a
//! confident arbitrage signal — the solver prices every trade against the
//! relation's own resolution states, so a wrong relation yields a wrong
//! guaranteed payoff. With an executor that can place real orders, that is the
//! sharpest edge in the system. Every group built here carries `inferred = true`
//! and the engine refuses to trade one with a live client unless a human has
//! explicitly allowed it.

use crate::{
    feed::rest::{Event, Market, Rest},
    fees::{FeeCategory, PolymarketFees},
    registry::Relation,
    types::{ContractId, Contracts, Venue},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

/// Errors returned by the discovery boundary.
///
/// The rest of the application already uses `anyhow::Error` for startup
/// composition, so this alias keeps discovery's public contract named without
/// throwing away the useful HTTP, JSON, and filesystem context at the CLI
/// boundary.
pub type DiscoveryError = anyhow::Error;

/// One venue listing, reduced to what inference and the registry need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawMarket {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    /// "greater", "less", "custom", or absent.
    pub strike_type: Option<String>,
    pub floor_strike: Option<f64>,
    pub cap_strike: Option<f64>,
    pub status: String,
    /// When trading stops, RFC3339. This is what ranking measures against.
    pub close_time: Option<String>,
}

/// One venue event, reduced the same way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEvent {
    pub event_ticker: String,
    pub series_ticker: String,
    pub title: String,
    pub mutually_exclusive: bool,
}

/// A point-in-time picture of the venue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Universe {
    /// Engine time when this was fetched, for the cache age check. Taken from
    /// the injected clock, never from the filesystem's idea of modification
    /// time, so replay and tests get the same answer every run.
    pub fetched_at_ms: u64,
    pub markets: Vec<RawMarket>,
    pub events: Vec<RawEvent>,
}

impl Universe {
    /// Markets grouped by event, each list sorted by ticker.
    ///
    /// Sorting is what makes discovery deterministic: contract ids are
    /// positional, so two runs that saw the same venue must intern in the same
    /// order or every id shifts.
    pub fn by_event(&self) -> BTreeMap<&str, Vec<&RawMarket>> {
        let mut grouped: BTreeMap<&str, Vec<&RawMarket>> = BTreeMap::new();
        for market in &self.markets {
            grouped
                .entry(market.event_ticker.as_str())
                .or_default()
                .push(market);
        }
        for markets in grouped.values_mut() {
            markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));
        }
        grouped
    }

    pub fn event(&self, ticker: &str) -> Option<&RawEvent> {
        self.events.iter().find(|e| e.event_ticker == ticker)
    }
}

impl From<Market> for RawMarket {
    fn from(market: Market) -> Self {
        RawMarket {
            ticker: market.ticker,
            event_ticker: market.event_ticker,
            title: market.title,
            strike_type: market.strike_type,
            floor_strike: market.floor_strike,
            cap_strike: market.cap_strike,
            status: market.status,
            close_time: market.close_time.or(market.expiration_time),
        }
    }
}

impl From<Event> for RawEvent {
    fn from(event: Event) -> Self {
        RawEvent {
            event_ticker: event.event_ticker,
            series_ticker: event.series_ticker,
            title: event.title,
            mutually_exclusive: event.mutually_exclusive,
        }
    }
}

/// How much of the venue to take.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryScope {
    /// Venue status filter. Empty takes everything.
    pub status: String,
    /// One series, such as `KXBTCD`. `None` walks the whole venue, which on
    /// Kalshi is over twelve thousand open markets.
    pub series: Option<String>,
    /// Bound on pagination, so a discovery run cannot become a crawl.
    pub max_pages: usize,
}

impl Default for DiscoveryScope {
    fn default() -> Self {
        DiscoveryScope {
            status: "open".into(),
            series: None,
            // 100 pages covers the current Kalshi universe at the venue's
            // 1,000-market / 200-event page sizes while retaining a hard stop
            // if the API ever returns a pathological cursor chain.
            max_pages: 100,
        }
    }
}

pub struct KalshiDiscovery {
    rest: Rest,
}

impl KalshiDiscovery {
    pub fn new(rest: Rest) -> Self {
        KalshiDiscovery { rest }
    }

    /// Fetch markets and the event metadata that explains them.
    pub async fn discover(
        &self,
        scope: &DiscoveryScope,
        now_ms: u64,
    ) -> std::result::Result<Universe, DiscoveryError> {
        let mut markets = match &scope.series {
            Some(series) => self.rest.series_markets(series).await?,
            None => {
                self.rest
                    .all_markets(&scope.status, scope.max_pages)
                    .await?
            }
        };
        // The series endpoint is walked through `/events` and does not accept
        // the status filter. Apply the same filter locally so a scoped cache
        // cannot accidentally include closed markets.
        if !scope.status.is_empty() {
            markets.retain(|market| market.status == scope.status);
        }
        let events = self.rest.all_events(&scope.status, scope.max_pages).await?;
        let mut universe = Universe {
            fetched_at_ms: now_ms,
            markets: markets.into_iter().map(RawMarket::from).collect(),
            events: events.into_iter().map(RawEvent::from).collect(),
        };
        // Deterministic order in, deterministic contract ids out.
        universe.markets.sort_by(|a, b| a.ticker.cmp(&b.ticker));
        universe
            .events
            .sort_by(|a, b| a.event_ticker.cmp(&b.event_ticker));
        Ok(universe)
    }
}

/// A gzipped snapshot of the venue on disk.
pub struct DiscoveryCache {
    path: PathBuf,
}

impl DiscoveryCache {
    pub fn new(cache_dir: &Path) -> Self {
        DiscoveryCache {
            path: cache_dir.join("kalshi_markets.json.gz"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reuse a fresh snapshot, otherwise fetch and store one.
    ///
    /// Age is measured against the snapshot's own recorded fetch time and the
    /// injected clock, not the file's modification time: a copied or restored
    /// file would otherwise look newly fetched.
    pub async fn load_or_fetch(
        &self,
        discovery: &KalshiDiscovery,
        scope: &DiscoveryScope,
        max_age_secs: u64,
        now_ms: u64,
    ) -> std::result::Result<Universe, DiscoveryError> {
        let path = self.path_for_scope(scope);
        if let Some(cached) = self.load_from(&path)? {
            let age_ms = now_ms.saturating_sub(cached.fetched_at_ms);
            if age_ms < max_age_secs.saturating_mul(1_000) {
                tracing::info!(
                    markets = cached.markets.len(),
                    age_secs = age_ms / 1_000,
                    "universe from cache"
                );
                return Ok(cached);
            }
        }
        let universe = discovery.discover(scope, now_ms).await?;
        self.save_to(&path, &universe)?;
        Ok(universe)
    }

    pub fn load(&self) -> std::result::Result<Option<Universe>, DiscoveryError> {
        self.load_from(&self.path)
    }

    fn load_from(&self, path: &Path) -> std::result::Result<Option<Universe>, DiscoveryError> {
        let Ok(file) = std::fs::File::open(path) else {
            return Ok(None);
        };
        let decoder = flate2::read::GzDecoder::new(file);
        // A corrupt cache is not fatal: discard it and refetch, rather than
        // refusing to start because a scratch file went bad.
        match serde_json::from_reader(decoder) {
            Ok(universe) => Ok(Some(universe)),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "discarding unreadable cache");
                Ok(None)
            }
        }
    }

    pub fn save(&self, universe: &Universe) -> std::result::Result<(), DiscoveryError> {
        self.save_to(&self.path, universe)
    }

    fn save_to(&self, path: &Path, universe: &Universe) -> std::result::Result<(), DiscoveryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file =
            std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        serde_json::to_writer(encoder, universe)?;
        Ok(())
    }

    /// A cache must not be shared by a whole-universe run and a series-scoped
    /// run.  Otherwise a recent BTC-only snapshot could silently become the
    /// answer to a later full-universe startup.  Keep the historical default
    /// filename for compatibility and suffix only non-default scopes.
    fn path_for_scope(&self, scope: &DiscoveryScope) -> PathBuf {
        if scope == &DiscoveryScope::default() {
            return self.path.clone();
        }
        let series = scope.series.as_deref().unwrap_or("all");
        let status = if scope.status.is_empty() {
            "all"
        } else {
            &scope.status
        };
        let suffix = format!(
            "{}-{}-{}",
            sanitize_path_part(status),
            sanitize_path_part(series),
            scope.max_pages
        );
        self.path
            .with_file_name(format!("kalshi_markets-{suffix}.json.gz"))
    }
}

fn sanitize_path_part(part: &str) -> String {
    part.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// What inference produced, and what it declined to touch.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct InferenceReport {
    pub markets: usize,
    pub events: usize,
    /// Events represented by one Kalshi binary market (the usual Yes/No pair).
    pub binary_events: usize,
    pub complements: usize,
    pub exhaustive_sets: usize,
    pub ladders: usize,
    /// Multi-market events whose structure the venue did not describe.
    pub events_not_inferable: usize,
}

/// A relation plus the tickers it relates, before interning.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InferredGroup {
    pub relation_kind: InferredKind,
    pub tickers: Vec<String>,
    pub event_ticker: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum InferredKind {
    Complement,
    Exhaustive,
    Monotone,
}

/// Read the constraint structure the venue already published.
pub fn infer_groups(universe: &Universe) -> (Vec<InferredGroup>, InferenceReport) {
    let mut groups = Vec::new();
    let mut report = InferenceReport {
        markets: universe.markets.len(),
        events: universe.by_event().len(),
        ..InferenceReport::default()
    };

    for (event_ticker, markets) in universe.by_event() {
        // Every binary market owes a dollar across its own two outcomes. This
        // holds whatever else the event turns out to be, so it is never wrong.
        for market in &markets {
            groups.push(InferredGroup {
                relation_kind: InferredKind::Complement,
                tickers: vec![market.ticker.clone()],
                event_ticker: event_ticker.to_string(),
            });
            report.complements += 1;
        }
        if markets.len() < 2 {
            report.binary_events += 1;
            continue;
        }

        let exclusive = universe
            .event(event_ticker)
            .is_some_and(|event| event.mutually_exclusive);
        if exclusive {
            groups.push(InferredGroup {
                relation_kind: InferredKind::Exhaustive,
                tickers: markets.iter().map(|m| m.ticker.clone()).collect(),
                event_ticker: event_ticker.to_string(),
            });
            report.exhaustive_sets += 1;
            // A mutually exclusive event is a partition, not a ladder. Reading
            // its brackets as nested claims would invert the relation and make
            // every adjacent pair look crossed.
            continue;
        }

        match ladder_order(&markets) {
            Some(ordered) => {
                groups.push(InferredGroup {
                    relation_kind: InferredKind::Monotone,
                    tickers: ordered,
                    event_ticker: event_ticker.to_string(),
                });
                report.ladders += 1;
            }
            None => report.events_not_inferable += 1,
        }
    }
    (groups, report)
}

/// Order a threshold ladder weakest claim first, if that is what this is.
///
/// "greater" rungs ascend by threshold, because a lower bar is easier to clear.
/// "less" rungs run the other way: "below 80,000" is more likely than "below
/// 70,000", so they descend. Anything mixed, unthresholded, or tied is not a
/// ladder this function is willing to name.
fn ladder_order(markets: &[&RawMarket]) -> Option<Vec<String>> {
    let kind = markets[0].strike_type.as_deref()?;
    if !markets
        .iter()
        .all(|m| m.strike_type.as_deref() == Some(kind))
    {
        return None;
    }
    let mut keyed: Vec<(f64, &str)> = match kind {
        "greater" => markets
            .iter()
            .map(|m| m.floor_strike.map(|s| (s, m.ticker.as_str())))
            .collect::<Option<Vec<_>>>()?,
        "less" => markets
            .iter()
            .map(|m| m.cap_strike.map(|s| (-s, m.ticker.as_str())))
            .collect::<Option<Vec<_>>>()?,
        // "between", "custom", and anything new: the venue did not describe a
        // nesting, so there is nothing to read.
        _ => return None,
    };
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    // Two rungs at one threshold are the same claim twice, which is a venue
    // listing oddity rather than a ladder.
    if keyed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    Some(keyed.into_iter().map(|(_, ticker)| ticker.into()).collect())
}

/// Intern the tickers an inferred group names, and turn it into a [`Relation`].
///
/// Returns `None` when a ticker is missing from the contract table, which means
/// the market was filtered out of the universe after inference ran.
pub fn to_relation(contracts: &Contracts, venue: Venue, group: &InferredGroup) -> Option<Relation> {
    let ids: Vec<ContractId> = group
        .tickers
        .iter()
        .map(|ticker| contracts.get(venue, ticker))
        .collect::<Option<Vec<_>>>()?;
    match group.relation_kind {
        InferredKind::Complement => ids.first().map(|contract| Relation::Complement {
            contract: *contract,
        }),
        InferredKind::Exhaustive => Some(Relation::Exhaustive { members: ids }),
        InferredKind::Monotone => Some(Relation::Monotone { ordered: ids }),
    }
}

/// Close times per event, for the resolution horizon ranking needs.
pub fn event_close_times(universe: &Universe) -> HashMap<String, String> {
    let mut closes: HashMap<String, String> = HashMap::new();
    for market in &universe.markets {
        let Some(close) = &market.close_time else {
            continue;
        };
        // The earliest close in an event is when its capital starts coming back.
        closes
            .entry(market.event_ticker.clone())
            .and_modify(|existing| {
                let is_earlier = match (
                    chrono::DateTime::parse_from_rfc3339(close),
                    chrono::DateTime::parse_from_rfc3339(existing),
                ) {
                    (Ok(left), Ok(right)) => left < right,
                    _ => close < existing,
                };
                if is_earlier {
                    *existing = close.clone();
                }
            })
            .or_insert_with(|| close.clone());
    }
    closes
}

/// Results of checking a discovered snapshot before it is handed to the
/// engine.  Discovery is intentionally conservative: a stale or structurally
/// ambiguous market is reported instead of being turned into a tradeable
/// relation by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveryValidation {
    pub markets: usize,
    pub events: usize,
    pub groups: usize,
    pub live_markets: usize,
    pub structural_errors: Vec<String>,
    pub closed_markets: Vec<String>,
    pub ladder_errors: Vec<String>,
}

impl DiscoveryValidation {
    pub fn is_valid(&self) -> bool {
        self.structural_errors.is_empty()
            && self.closed_markets.is_empty()
            && self.ladder_errors.is_empty()
    }
}

/// Validate the portions of Kalshi metadata that inference relies on.
///
/// This deliberately does not require every market to have a second market in
/// its event: Kalshi's normal binary market is one listing with two outcomes,
/// and its complement relation is therefore represented by one contract. The
/// event-level exhaustive and ladder checks cover multi-market structures.
pub fn validate_discovered_universe(universe: &Universe, now_ms: u64) -> DiscoveryValidation {
    let (groups, _) = infer_groups(universe);
    let mut result = DiscoveryValidation {
        markets: universe.markets.len(),
        events: universe.by_event().len(),
        groups: groups.len(),
        live_markets: 0,
        structural_errors: Vec::new(),
        closed_markets: Vec::new(),
        ladder_errors: Vec::new(),
    };

    let by_ticker: HashMap<&str, &RawMarket> = universe
        .markets
        .iter()
        .map(|market| (market.ticker.as_str(), market))
        .collect();

    for market in &universe.markets {
        let status = market.status.to_ascii_lowercase();
        let status_live = matches!(
            status.as_str(),
            "open" | "active" | "initialized" | "paused"
        );
        let close_live = market
            .close_time
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .is_some_and(|close| u64::try_from(close.timestamp_millis()).unwrap_or(0) > now_ms);
        if status_live && close_live {
            result.live_markets += 1;
        } else {
            result.closed_markets.push(market.ticker.clone());
        }
    }

    for group in &groups {
        if group.tickers.is_empty() {
            result
                .structural_errors
                .push(format!("{} group has no members", group.event_ticker));
            continue;
        }
        let members: Vec<&RawMarket> = group
            .tickers
            .iter()
            .filter_map(|ticker| by_ticker.get(ticker.as_str()).copied())
            .collect();
        if members.len() != group.tickers.len() {
            result.structural_errors.push(format!(
                "{} {:?} group references a market not present in the snapshot",
                group.event_ticker, group.relation_kind
            ));
            continue;
        }
        if members
            .iter()
            .any(|market| market.event_ticker != group.event_ticker)
        {
            result.structural_errors.push(format!(
                "{} group contains a market from another event",
                group.event_ticker
            ));
        }

        match group.relation_kind {
            InferredKind::Complement => {}
            InferredKind::Exhaustive => {
                if group.tickers.len() < 2
                    || !universe
                        .event(&group.event_ticker)
                        .is_some_and(|event| event.mutually_exclusive)
                {
                    result.structural_errors.push(format!(
                        "{} exhaustive group is not backed by a mutually exclusive event",
                        group.event_ticker
                    ));
                }
            }
            InferredKind::Monotone => {
                if let Some(error) = validate_ladder_members(&members) {
                    result
                        .ladder_errors
                        .push(format!("{} ladder: {error}", group.event_ticker));
                }
            }
        }
    }
    result
}

fn validate_ladder_members(markets: &[&RawMarket]) -> Option<String> {
    let first_kind = markets.first()?.strike_type.as_deref()?;
    if !markets
        .iter()
        .all(|market| market.strike_type.as_deref() == Some(first_kind))
    {
        return Some("strike types are mixed".into());
    }
    let mut ordered: Vec<f64> = match first_kind {
        "greater" => markets
            .iter()
            .filter_map(|market| market.floor_strike)
            .collect(),
        "less" => markets
            .iter()
            .filter_map(|market| market.cap_strike)
            .map(|strike| -strike)
            .collect(),
        _ => return Some(format!("unsupported strike type {first_kind:?}")),
    };
    if ordered.len() != markets.len() {
        return Some("a rung has no strike".into());
    }
    ordered.sort_by(|a, b| a.total_cmp(b));
    if ordered.windows(2).any(|pair| pair[0] == pair[1]) {
        return Some("two rungs have the same strike".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::WallClock, feed::kalshi::Environment};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn market(ticker: &str, event: &str, strike: Option<f64>) -> RawMarket {
        RawMarket {
            ticker: ticker.into(),
            event_ticker: event.into(),
            title: "t".into(),
            strike_type: strike.map(|_| "greater".into()),
            floor_strike: strike,
            cap_strike: None,
            status: "active".into(),
            close_time: Some("2026-09-20T21:00:00Z".into()),
        }
    }

    fn universe(markets: Vec<RawMarket>, events: Vec<RawEvent>) -> Universe {
        Universe {
            fetched_at_ms: 1_000,
            markets,
            events,
        }
    }

    fn event(ticker: &str, exclusive: bool) -> RawEvent {
        RawEvent {
            event_ticker: ticker.into(),
            series_ticker: "S".into(),
            title: "e".into(),
            mutually_exclusive: exclusive,
        }
    }

    #[test]
    fn every_binary_market_is_its_own_complement() {
        let u = universe(
            vec![market("A-1", "A", None), market("B-1", "B", None)],
            vec![event("A", false), event("B", false)],
        );
        let (groups, report) = infer_groups(&u);
        assert_eq!(report.complements, 2);
        assert_eq!(report.exhaustive_sets, 0);
        assert_eq!(report.ladders, 0);
        assert!(
            groups
                .iter()
                .all(|g| g.relation_kind == InferredKind::Complement && g.tickers.len() == 1)
        );
    }

    #[test]
    fn a_mutually_exclusive_event_is_an_exhaustive_set_and_never_a_ladder() {
        // Seven papal candidates, no strikes: exactly one resolves yes.
        let markets: Vec<RawMarket> = ["PPAR", "PPIZ", "PERD"]
            .iter()
            .map(|s| market(&format!("POPE-{s}"), "POPE", None))
            .collect();
        let (groups, report) = infer_groups(&universe(markets, vec![event("POPE", true)]));
        assert_eq!(report.exhaustive_sets, 1);
        assert_eq!(report.ladders, 0);
        let set = groups
            .iter()
            .find(|g| g.relation_kind == InferredKind::Exhaustive)
            .unwrap();
        assert_eq!(set.tickers.len(), 3);

        // Even with strikes, exclusivity wins: brackets are a partition, and
        // reading them as nested claims would invert the relation.
        let bracketed: Vec<RawMarket> = [(1.0, "a"), (2.0, "b"), (3.0, "c")]
            .iter()
            .map(|(s, n)| market(&format!("BR-{n}"), "BR", Some(*s)))
            .collect();
        let (_, report) = infer_groups(&universe(bracketed, vec![event("BR", true)]));
        assert_eq!(report.exhaustive_sets, 1);
        assert_eq!(report.ladders, 0);
    }

    #[test]
    fn threshold_rungs_become_a_ladder_ordered_weakest_first() {
        // Deliberately out of order on the wire.
        let markets = vec![
            market("BTC-T72000", "BTC", Some(71_999.99)),
            market("BTC-T71600", "BTC", Some(71_599.99)),
            market("BTC-T71800", "BTC", Some(71_799.99)),
        ];
        let (groups, report) = infer_groups(&universe(markets, vec![event("BTC", false)]));
        assert_eq!(report.ladders, 1);
        assert_eq!(report.complements, 3, "each rung is still a complement");
        let ladder = groups
            .iter()
            .find(|g| g.relation_kind == InferredKind::Monotone)
            .unwrap();
        // Lowest threshold first: "above 71,600" is the easiest claim.
        assert_eq!(ladder.tickers, ["BTC-T71600", "BTC-T71800", "BTC-T72000"]);
    }

    #[test]
    fn a_less_than_ladder_runs_the_other_way() {
        let below = |ticker: &str, cap: f64| RawMarket {
            strike_type: Some("less".into()),
            floor_strike: None,
            cap_strike: Some(cap),
            ..market(ticker, "IDX", None)
        };
        let markets = vec![below("IDX-L70", 70.0), below("IDX-L80", 80.0)];
        let (groups, _) = infer_groups(&universe(markets, vec![event("IDX", false)]));
        let ladder = groups
            .iter()
            .find(|g| g.relation_kind == InferredKind::Monotone)
            .unwrap();
        // "below 80" is the likelier claim, so it leads.
        assert_eq!(ladder.tickers, ["IDX-L80", "IDX-L70"]);
    }

    #[test]
    fn structures_the_venue_did_not_describe_are_left_alone() {
        let custom = |ticker: &str| RawMarket {
            strike_type: Some("custom".into()),
            ..market(ticker, "X", None)
        };
        let (groups, report) = infer_groups(&universe(
            vec![custom("X-1"), custom("X-2")],
            vec![event("X", false)],
        ));
        assert_eq!(report.events_not_inferable, 1);
        assert_eq!(report.ladders, 0);
        assert_eq!(report.exhaustive_sets, 0);
        // The complements still stand; they never depended on the structure.
        assert_eq!(report.complements, 2);
        assert!(
            groups
                .iter()
                .all(|g| g.relation_kind == InferredKind::Complement)
        );

        // Mixed strike types are not a ladder either.
        let mixed = vec![
            RawMarket {
                strike_type: Some("greater".into()),
                floor_strike: Some(1.0),
                ..market("M-1", "M", None)
            },
            custom("M-2"),
        ];
        let (_, report) = infer_groups(&universe(mixed, vec![event("M", false)]));
        assert_eq!(report.ladders, 0);

        // Two rungs at one threshold are the same claim twice.
        let duplicate = vec![market("D-1", "D", Some(5.0)), market("D-2", "D", Some(5.0))];
        let (_, report) = infer_groups(&universe(duplicate, vec![event("D", false)]));
        assert_eq!(report.ladders, 0);
    }

    #[test]
    fn an_empty_universe_infers_nothing_rather_than_erroring() {
        let (groups, report) = infer_groups(&universe(Vec::new(), Vec::new()));
        assert!(groups.is_empty());
        assert_eq!(report, InferenceReport::default());
    }

    #[test]
    fn inference_is_deterministic_regardless_of_wire_order() {
        let build = |order: [usize; 3]| {
            let all = [
                market("BTC-T72000", "BTC", Some(71_999.99)),
                market("BTC-T71600", "BTC", Some(71_599.99)),
                market("BTC-T71800", "BTC", Some(71_799.99)),
            ];
            let markets = order.iter().map(|i| all[*i].clone()).collect();
            infer_groups(&universe(markets, vec![event("BTC", false)])).0
        };
        // Contract ids are positional, so two runs of the same venue must
        // produce the same order or every id shifts underneath the book store.
        assert_eq!(build([0, 1, 2]), build([2, 0, 1]));
        assert_eq!(build([0, 1, 2]), build([1, 2, 0]));
    }

    #[test]
    fn the_cache_round_trips_and_survives_a_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("sum100-disc-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = DiscoveryCache::new(&dir);
        assert!(cache.load().unwrap().is_none(), "no file yet");

        let u = universe(vec![market("A-1", "A", None)], vec![event("A", false)]);
        cache.save(&u).unwrap();
        assert_eq!(cache.load().unwrap().unwrap(), u);

        // Garbage is discarded and refetched rather than failing startup.
        std::fs::write(cache.path(), b"not gzip").unwrap();
        assert!(cache.load().unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn non_default_scopes_use_separate_cache_files() {
        let dir = std::env::temp_dir().join(format!("sum100-disc-scope-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = DiscoveryCache::new(&dir);
        let default_path = cache.path().to_owned();
        let scoped = DiscoveryScope {
            series: Some("KXBTCD".into()),
            ..DiscoveryScope::default()
        };
        assert_ne!(
            cache.path_for_scope(&scoped),
            default_path,
            "a series cache must not masquerade as a full-universe cache"
        );
        assert!(
            cache
                .path_for_scope(&scoped)
                .to_string_lossy()
                .contains("KXBTCD")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovered_validation_rejects_closed_or_expired_markets() {
        let mut closed = market("A-1", "A", None);
        closed.status = "closed".into();
        let validation =
            validate_discovered_universe(&universe(vec![closed], vec![event("A", false)]), 1_000);
        assert!(!validation.is_valid());
        assert_eq!(validation.live_markets, 0);
        assert_eq!(validation.closed_markets, ["A-1"]);

        let mut expired = market("A-1", "A", None);
        expired.close_time = Some("1970-01-01T00:00:00Z".into());
        let validation =
            validate_discovered_universe(&universe(vec![expired], vec![event("A", false)]), 1_000);
        assert!(!validation.is_valid());
        assert_eq!(validation.closed_markets, ["A-1"]);
    }

    #[test]
    fn discovered_validation_accepts_a_live_binary_market() {
        let mut live = market("A-1", "A", None);
        live.status = "open".into();
        let validation =
            validate_discovered_universe(&universe(vec![live], vec![event("A", false)]), 1_000);
        assert!(validation.is_valid(), "{validation:?}");
        assert_eq!(validation.groups, 1);
        assert_eq!(validation.live_markets, 1);
    }

    #[test]
    fn auto_registry_interns_markets_and_indexes_inferred_groups() {
        let markets = vec![
            market("BTC-T72000", "BTC", Some(72_000.0)),
            market("BTC-T71600", "BTC", Some(71_600.0)),
            market("BTC-T71800", "BTC", Some(71_800.0)),
        ];
        let registry =
            crate::registry::Registry::from_universe(&universe(markets, vec![event("BTC", false)]))
                .unwrap();
        assert_eq!(
            registry.tickers(Venue::Kalshi),
            ["BTC-T71600", "BTC-T71800", "BTC-T72000",]
        );
        assert_eq!(
            registry.groups().len(),
            4,
            "three complements plus one ladder"
        );
        assert_eq!(registry.groups_for(ContractId(0)).len(), 2);
        assert_eq!(registry.inferred_count(), 4);
    }

    #[tokio::test]
    async fn stale_cache_refetches_from_the_rest_api() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 4096];
                let size = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                let body = if request.starts_with("GET /markets") {
                    r#"{"markets":[{"ticker":"A-1","event_ticker":"A","title":"A","status":"open","close_time":"2099-01-01T00:00:00Z"}],"cursor":""}"#
                } else {
                    r#"{"events":[{"event_ticker":"A","series_ticker":"S","title":"A","mutually_exclusive":false}],"cursor":""}"#
                };
                server_calls.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let dir = std::env::temp_dir().join(format!("sum100-disc-http-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = DiscoveryCache::new(&dir);
        let stale = universe(Vec::new(), Vec::new());
        cache.save(&stale).unwrap();
        let rest = Rest::with_base_url(
            Environment::Demo,
            format!("http://{}", address),
            Arc::new(WallClock),
        )
        .unwrap();
        let discovery = KalshiDiscovery::new(rest);
        let result = cache
            .load_or_fetch(&discovery, &DiscoveryScope::default(), 1, 5_000)
            .await
            .unwrap();
        assert_eq!(result.markets.len(), 1);
        assert_eq!(result.events.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ------------------------------------------------------------------ Polymarket

/// A Polymarket market as discovery sees it.
///
/// One market is one engine contract, identified by its yes token. The no token
/// is the same order book mirrored — an ask at `p` on one side is a bid at
/// `100 - p` on the other, at the same size — so carrying both would double
/// every book and produce a "group" that is coherent by construction and can
/// never signal. See [`crate::feed::polymarket`] for the evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct PolymarketMarket {
    pub yes_token: String,
    pub no_token: String,
    pub question: String,
    pub tick_size: f64,
    /// Gamma's `feeType`, absent when the market has no schedule attached.
    pub fee_type: Option<String>,
    pub fees_enabled: bool,
    /// ISO 8601, as the venue publishes it.
    pub ends_at: Option<String>,
}

impl PolymarketMarket {
    /// What this market charges a taker.
    ///
    /// `feesEnabled` is authoritative for *whether* there is a fee; `feeType`
    /// only says which schedule. A market with fees on but no named schedule is
    /// an unknown, and an unknown fee is read as the dearest one rather than as
    /// free, on the same reasoning as [`FeeCategory::from_fee_type`].
    pub fn fee_category(&self) -> FeeCategory {
        if !self.fees_enabled {
            return FeeCategory::Zero;
        }
        match self.fee_type.as_deref() {
            Some(fee_type) => FeeCategory::from_fee_type(fee_type),
            None => FeeCategory::Crypto,
        }
    }

    /// Whether this engine can represent the market's prices.
    ///
    /// A finer tick cannot be held in a book indexed by whole cents, and
    /// rounding one into range would quote a price nobody made.
    pub fn tick_is_representable(&self) -> bool {
        (self.tick_size - 0.01).abs() < 1e-9
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    #[serde(default)]
    question: String,
    #[serde(default)]
    clob_token_ids: Option<String>,
    #[serde(default)]
    order_price_min_tick_size: Option<f64>,
    #[serde(default)]
    fee_type: Option<String>,
    #[serde(default)]
    fees_enabled: bool,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    accepting_orders: bool,
    #[serde(default)]
    end_date: Option<String>,
}

/// Reads the market catalogue from Gamma.
///
/// Public metadata: no credentials, and nothing here can place an order.
pub struct PolymarketDiscovery {
    base_url: String,
    client: reqwest::Client,
}

impl PolymarketDiscovery {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()?,
        })
    }

    /// One page of markets this engine could trade.
    ///
    /// A market that does not parse is skipped rather than failing the page.
    /// The catalogue is large and partly historical, and one entry missing a
    /// field is not a reason to discover nothing.
    pub async fn fetch_page(&self, limit: usize, offset: usize) -> Result<Vec<PolymarketMarket>> {
        let url = format!(
            "{}/markets?limit={limit}&offset={offset}&closed=false&order=volume24hr&ascending=false",
            self.base_url
        );
        let response = self.client.get(&url).send().await?;
        ensure!(
            response.status().is_success(),
            "gamma returned {} for {url}",
            response.status()
        );
        let raw: Vec<GammaMarket> = response.json().await?;
        Ok(raw.iter().filter_map(Self::market).collect())
    }

    fn market(raw: &GammaMarket) -> Option<PolymarketMarket> {
        if raw.closed || !raw.accepting_orders {
            return None;
        }
        // Two tokens, and the array arrives as a JSON string inside the JSON.
        let encoded = raw.clob_token_ids.as_deref()?;
        let tokens: Vec<String> = serde_json::from_str(encoded).ok()?;
        let [yes_token, no_token] = <[String; 2]>::try_from(tokens).ok()?;
        Some(PolymarketMarket {
            yes_token,
            no_token,
            question: raw.question.clone(),
            tick_size: raw.order_price_min_tick_size?,
            fee_type: raw.fee_type.clone(),
            fees_enabled: raw.fees_enabled,
            ends_at: raw.end_date.clone(),
        })
    }

    /// Every tradeable market this engine can represent, most active first.
    ///
    /// Stops at `max_markets` rather than walking the whole venue: the
    /// catalogue runs to thousands, and subscribing to all of them is a
    /// feed-capacity decision rather than a discovery one.
    pub async fn fetch_representable(&self, max_markets: usize) -> Result<Vec<PolymarketMarket>> {
        const PAGE: usize = 100;
        let mut found = Vec::new();
        let mut offset = 0;
        while found.len() < max_markets {
            let page = self.fetch_page(PAGE, offset).await?;
            if page.is_empty() {
                break;
            }
            found.extend(
                page.into_iter()
                    .filter(PolymarketMarket::tick_is_representable),
            );
            offset += PAGE;
        }
        found.truncate(max_markets);
        Ok(found)
    }
}

/// One complement group per market: a binary market owes a dollar across its
/// own two outcomes, which holds whatever else the market turns out to be.
///
/// Not an exhaustive pair over the two tokens. Those are the same book seen
/// from both sides, so such a group sums to exactly a dollar by construction
/// and could never report an incoherence.
pub fn polymarket_complement_groups(markets: &[PolymarketMarket]) -> Vec<InferredGroup> {
    markets
        .iter()
        .map(|market| InferredGroup {
            relation_kind: InferredKind::Complement,
            tickers: vec![market.yes_token.clone()],
            event_ticker: market.yes_token.clone(),
        })
        .collect()
}

/// Record what each market charges, against the contract its token interned to.
///
/// Returns how many were registered. A market whose token this engine has not
/// interned is skipped, and keeps paying the conservative default until it is.
pub fn register_polymarket_fees(
    markets: &[PolymarketMarket],
    contracts: &Contracts,
    fees: &mut PolymarketFees,
) -> usize {
    let mut registered = 0;
    for market in markets {
        let Some(contract) = contracts.get(Venue::Polymarket, &market.yes_token) else {
            continue;
        };
        fees.register_category(contract, market.fee_category());
        registered += 1;
    }
    registered
}
