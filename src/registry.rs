//! Constraint registry: which contracts are logically related, and how.
//!
//! This module owns three things the solver cannot work without: the shape of a
//! constraint group, the *meaning* of each relation expressed as the set of
//! resolutions it permits, and the reverse index from a contract to the groups
//! containing it. Loading `config/registry.toml` and reconciling it against
//! venue market metadata is phase 5; nothing here reads a file.
//!
//! The reverse index exists because dirty marking must be cheap. A price update
//! touches one or two groups out of thousands, and finding them has to be a
//! hash lookup rather than a scan, so the index is built once at load time.
//!
//! # Loading is offline; verifying against the venue is not
//!
//! [`Registry::from_toml`] touches no network. It parses the file, interns every
//! ticker into [`ContractId`]s, builds the groups and the index, and enforces
//! every invariant that can be checked from the file alone. Confirming that
//! those tickers are real markets is [`Registry::validate_against_markets`],
//! which takes metadata the caller already fetched.
//!
//! The split is deliberate. Replay is required to run with no credentials and no
//! network, and it has to resolve the same tickers to the same contract ids as
//! the live run did, or the recorded stream would land in the wrong books. A
//! loader that phoned a venue would break both properties. Interning is
//! deterministic, so the ids fall out of the file's own ordering.
//!
//! # Contract ids are positional, and that couples three things
//!
//! [`Contracts::intern`] assigns ids in call order, and both
//! [`crate::feed::kalshi::Parser`] and [`crate::book::BookStore`] intern from a
//! ticker list. For their ids to agree with the registry's, all three must see
//! the same list in the same order — which is why [`Registry::tickers`] exists
//! and why the loader interns every Kalshi member before any Polymarket one.
//! `registry_parser_and_book_store_agree_on_contract_ids` pins it.

use crate::{
    clock::Clock,
    types::{ContractId, Contracts, Side, Venue},
};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Registry-scoped group identifier. Ordering is derived so the solver can sort
/// by it and produce the same output order on every replay of the same input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupId(pub u32);

/// One way the world can resolve, as the set of member contracts paying $1.
///
/// Anything not in the set pays nothing. This is the entire semantic content of
/// a [`Relation`]: two relations that permit the same resolutions are the same
/// constraint, and a trade is risk-free exactly when its payoff is acceptable
/// under every state in this list.
pub type ResolutionState = Vec<ContractId>;

/// A logical relationship between contracts that constrains their prices.
///
/// [`Monotone`](Relation::Monotone) is a generalization of
/// [`Implies`](Relation::Implies), not a separate idea: a ladder of `n` rungs is
/// `n - 1` chained implications, and expressing it as one group means a rung
/// appears once rather than twice and the solver evaluates the whole ladder on a
/// single dirty mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    /// A binary contract and its own negation: yes and no sum to exactly $1.
    Complement { contract: ContractId },
    /// Mutually exclusive and exhaustive outcomes: exactly one pays $1.
    Exhaustive { members: Vec<ContractId> },
    /// A threshold ladder, ordered from the weakest claim to the strongest.
    ///
    /// Each rung's yes event contains the next rung's, so probabilities must be
    /// non-increasing along the vector. "BTC above 70k" comes before "BTC above
    /// 80k".
    Monotone { ordered: Vec<ContractId> },
    /// `antecedent` resolving yes forces `consequent` to resolve yes.
    Implies {
        antecedent: ContractId,
        consequent: ContractId,
    },
    /// The same real-world event listed on two venues.
    ///
    /// `verified` stays false until a human has confirmed both contracts settle
    /// on the same source, at the same time, with the same tie handling. An
    /// unverified pair never produces a signal: a cross-venue trade on two
    /// contracts that turn out to differ is not an arbitrage, it is a naked
    /// directional position taken by accident.
    Equivalent {
        a: ContractId,
        b: ContractId,
        verified: bool,
    },
}

impl Relation {
    /// Every contract this relation constrains, in the relation's own order.
    pub fn members(&self) -> Vec<ContractId> {
        match self {
            Relation::Complement { contract } => vec![*contract],
            Relation::Exhaustive { members } => members.clone(),
            Relation::Monotone { ordered } => ordered.clone(),
            Relation::Implies {
                antecedent,
                consequent,
            } => vec![*antecedent, *consequent],
            Relation::Equivalent { a, b, .. } => vec![*a, *b],
        }
    }

    /// Enumerate every resolution the relation permits.
    ///
    /// This is what makes the payoff guarantee checkable rather than asserted.
    /// The solver prices a trade against the worst state in this list, so a
    /// relation that is wrong here produces a position that looks risk-free and
    /// is not — which is why the relation semantics live in one place and every
    /// fast path is priced through them instead of hardcoding "payoff is 100".
    pub fn resolution_states(&self) -> Vec<ResolutionState> {
        match self {
            Relation::Complement { contract } => vec![vec![*contract], Vec::new()],
            Relation::Exhaustive { members } => members.iter().map(|m| vec![*m]).collect(),
            // A ladder admits exactly one state per cut point: the first k rungs
            // resolve yes and the rest no. Any other combination would break the
            // containment that defines the ladder.
            Relation::Monotone { ordered } => (0..=ordered.len())
                .map(|cut| ordered[..cut].to_vec())
                .collect(),
            Relation::Implies {
                antecedent,
                consequent,
            } => vec![
                Vec::new(),
                vec![*consequent],
                vec![*consequent, *antecedent],
            ],
            Relation::Equivalent { a, b, .. } => vec![vec![*a, *b], Vec::new()],
        }
    }
}

/// A relation plus the identity and timing the solver needs to rank it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintGroup {
    pub id: GroupId,
    pub relation: Relation,
    /// Derived from `relation` at construction; never set independently, so the
    /// two cannot drift apart.
    members: Vec<ContractId>,
    /// When the underlying event settles and the locked capital comes back.
    /// Ranking is meaningless without it, so it is required rather than optional.
    pub resolves_at_ms: u64,
    /// True when this group was read off venue metadata rather than written by
    /// a human. An inferred relation that is wrong does not go quiet, it emits
    /// a confident arbitrage, so the engine refuses to trade one with a live
    /// order client unless that is explicitly allowed.
    pub inferred: bool,
}

impl ConstraintGroup {
    pub fn members(&self) -> &[ContractId] {
        &self.members
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// A set, ladder, or pair that does not have enough distinct members to
    /// constrain anything.
    TooFewMembers { group: GroupId, got: usize },
    /// The same contract listed twice in one group. Almost always a config typo,
    /// and it would silently double-count depth on one book.
    DuplicateMember {
        group: GroupId,
        contract: ContractId,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::TooFewMembers { group, got } => {
                write!(f, "group {} has only {got} member(s)", group.0)
            }
            RegistryError::DuplicateMember { group, contract } => {
                write!(f, "group {} lists contract {} twice", group.0, contract.0)
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Interned event identifier. Like [`ContractId`], the integer is positional and
/// the human-readable key lives on the record it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct EventId(pub u32);

/// One real-world question, independent of how any venue lists it.
///
/// `resolves_at` is what makes ranking possible: capital in a prediction market
/// is trapped until the event settles, so a trade's tenor is as much a part of
/// its value as its edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalEvent {
    pub id: EventId,
    /// The `id` string from the file, kept for logs and error messages.
    pub key: String,
    pub description: String,
    pub resolves_at: DateTime<Utc>,
    pub resolution_source: String,
    /// Optional grouping for risk limits: every Fed decision shares a theme
    /// because they share a resolution source and would fail together.
    pub theme: Option<String>,
    /// Placeholder for resolution-rule versioning, which lands in phase 8. A
    /// change here will mean "the venue rewrote the rules, re-verify the pair".
    pub resolution_rules_hash: u64,
}

/// One venue listing bound to a canonical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractBinding {
    pub contract_id: ContractId,
    pub venue: Venue,
    /// The venue's own identifier: a Kalshi ticker or a Polymarket token.
    pub venue_ticker: String,
    pub event: EventId,
    /// Which outcome of the venue listing corresponds to the event resolving
    /// yes. Defaults to yes; a venue that lists only the negation sets no.
    pub side: Side,
    /// False until a human has confirmed this listing settles on the same
    /// source, at the same time, with the same tie handling as the event.
    pub verified: bool,
}

/// Constraint groups, the contract-to-group reverse index, and the identity
/// tables the groups are built from.
#[derive(Debug, Default)]
pub struct Registry {
    events: Vec<CanonicalEvent>,
    event_by_key: HashMap<String, EventId>,
    bindings: HashMap<ContractId, ContractBinding>,
    contracts: Contracts,
    groups: Vec<ConstraintGroup>,
    by_contract: HashMap<ContractId, Vec<GroupId>>,
    /// Interned tickers per venue, in id order.
    tickers: HashMap<Venue, Vec<String>>,
}

impl Registry {
    /// Build a registry from relations, assigning [`GroupId`]s in input order.
    ///
    /// Validation happens here rather than at query time because a malformed
    /// group is a configuration mistake, and the only useful moment to discover
    /// it is before the engine starts trading on it.
    pub fn new(
        relations: impl IntoIterator<Item = (Relation, u64)>,
    ) -> Result<Self, RegistryError> {
        let mut registry = Registry::default();
        for (relation, resolves_at_ms) in relations {
            registry.push_group(relation, resolves_at_ms, false)?;
        }
        Ok(registry)
    }

    /// Validate one relation's shape and file it into the groups and the index.
    fn push_group(
        &mut self,
        relation: Relation,
        resolves_at_ms: u64,
        inferred: bool,
    ) -> Result<GroupId, RegistryError> {
        let id = GroupId(u32::try_from(self.groups.len()).unwrap_or(u32::MAX));
        let members = relation.members();
        if members.len() < 2 && !matches!(relation, Relation::Complement { .. }) {
            return Err(RegistryError::TooFewMembers {
                group: id,
                got: members.len(),
            });
        }
        for (i, contract) in members.iter().enumerate() {
            if members[..i].contains(contract) {
                return Err(RegistryError::DuplicateMember {
                    group: id,
                    contract: *contract,
                });
            }
        }
        for contract in &members {
            self.by_contract.entry(*contract).or_default().push(id);
        }
        self.groups.push(ConstraintGroup {
            id,
            relation,
            members,
            resolves_at_ms,
            inferred,
        });
        Ok(id)
    }

    pub fn groups(&self) -> &[ConstraintGroup] {
        &self.groups
    }

    pub fn group(&self, id: GroupId) -> Option<&ConstraintGroup> {
        self.groups.get(id.0 as usize)
    }

    /// Groups containing this contract. The reverse index that makes dirty
    /// marking a hash lookup instead of a scan over every group.
    pub fn groups_for(&self, contract: ContractId) -> &[GroupId] {
        self.by_contract
            .get(&contract)
            .map_or(&[][..], Vec::as_slice)
    }

    pub fn events(&self) -> &[CanonicalEvent] {
        &self.events
    }

    pub fn event(&self, id: EventId) -> Option<&CanonicalEvent> {
        self.events.get(id.0 as usize)
    }

    pub fn event_by_key(&self, key: &str) -> Option<&CanonicalEvent> {
        self.event_by_key.get(key).and_then(|id| self.event(*id))
    }

    pub fn binding(&self, contract: ContractId) -> Option<&ContractBinding> {
        self.bindings.get(&contract)
    }

    pub fn bindings(&self) -> impl Iterator<Item = &ContractBinding> {
        self.bindings.values()
    }

    pub fn contracts(&self) -> &Contracts {
        &self.contracts
    }

    /// Tickers for one venue, in the order their [`ContractId`]s were assigned.
    ///
    /// Hand this list to the parser and the book store. Passing anything else,
    /// including the same tickers in another order, silently shifts every id.
    pub fn tickers(&self, venue: Venue) -> &[String] {
        self.tickers.get(&venue).map_or(&[][..], Vec::as_slice)
    }
}

// -------------------------------------------------------------- the TOML file

/// `config/registry.toml`, exactly as written. Unknown keys are rejected so a
/// misspelled `verifed = true` fails at startup instead of quietly leaving a
/// cross-venue pair unverified.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(default)]
    event: Vec<EventEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventEntry {
    id: String,
    description: String,
    resolves_at: String,
    resolution_source: String,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    resolution_rules_hash: u64,
    #[serde(default)]
    group: Vec<GroupEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupEntry {
    #[serde(rename = "type")]
    kind: String,
    /// Required on `equivalent` and rejected everywhere else: a cross-venue pair
    /// must state its verification status rather than inherit a default.
    verified: Option<bool>,
    members: Vec<MemberRef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberRef {
    venue: String,
    ticker: Option<String>,
    token: Option<String>,
    side: Option<String>,
}

impl MemberRef {
    /// Normalize a member to the venue, its identifier, and the outcome bound.
    fn resolve(&self) -> Result<(Venue, String, Side)> {
        let venue = match self.venue.as_str() {
            "kalshi" => Venue::Kalshi,
            "polymarket" => Venue::Polymarket,
            other => bail!("unknown venue {other:?}; expected kalshi or polymarket"),
        };
        // Kalshi identifies a market by ticker, Polymarket by token. One or the
        // other, never both, so a member cannot name two different markets.
        let key = match (&self.ticker, &self.token) {
            (Some(ticker), None) => ticker.clone(),
            (None, Some(token)) => token.clone(),
            (Some(_), Some(_)) => bail!("member has both ticker and token"),
            (None, None) => bail!("member has neither ticker nor token"),
        };
        ensure!(!key.trim().is_empty(), "member identifier is empty");
        let side = match self.side.as_deref() {
            None | Some("yes") => Side::Yes,
            Some("no") => Side::No,
            Some(other) => bail!("unknown side {other:?}; expected yes or no"),
        };
        Ok((venue, key, side))
    }
}

impl Registry {
    /// Load the constraint graph from a TOML file. No network, no clock.
    pub fn from_toml(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading registry {}", path.display()))?;
        Registry::parse(&text).with_context(|| format!("parsing registry {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: RegistryFile = toml::from_str(text)?;
        ensure!(!file.event.is_empty(), "registry defines no events");
        let mut registry = Registry::default();

        // Pass one interns identity only, every Kalshi member before any
        // Polymarket one, so a Kalshi-only book store built from
        // `tickers(Kalshi)` assigns exactly these ids. See the module docs.
        for venue in [Venue::Kalshi, Venue::Polymarket] {
            for event in &file.event {
                for group in &event.group {
                    for member in &group.members {
                        let (member_venue, key, _) = member
                            .resolve()
                            .with_context(|| format!("event {}", event.id))?;
                        if member_venue != venue || registry.contracts.get(venue, &key).is_some() {
                            continue;
                        }
                        registry.contracts.intern(venue, &key)?;
                        registry.tickers.entry(venue).or_default().push(key);
                    }
                }
            }
        }

        // Pass two builds events, bindings, and groups against those ids.
        for entry in &file.event {
            let event_id = EventId(u32::try_from(registry.events.len())?);
            ensure!(
                !registry.event_by_key.contains_key(&entry.id),
                "duplicate event id {:?}",
                entry.id
            );
            let resolves_at = DateTime::parse_from_rfc3339(&entry.resolves_at)
                .with_context(|| {
                    format!(
                        "event {}: resolves_at {:?} is not RFC3339",
                        entry.id, entry.resolves_at
                    )
                })?
                .with_timezone(&Utc);
            registry.events.push(CanonicalEvent {
                id: event_id,
                key: entry.id.clone(),
                description: entry.description.clone(),
                resolves_at,
                resolution_source: entry.resolution_source.clone(),
                theme: entry.theme.clone(),
                resolution_rules_hash: entry.resolution_rules_hash,
            });
            registry.event_by_key.insert(entry.id.clone(), event_id);

            // Negative milliseconds would mean an event that resolved before the
            // Unix epoch. Clamp rather than wrap; the freshness gate and the
            // horizon floor both handle a past resolution already.
            let resolves_at_ms = u64::try_from(resolves_at.timestamp_millis()).unwrap_or(0);

            for group in &entry.group {
                registry
                    .add_group(event_id, &entry.id, group, resolves_at_ms)
                    .with_context(|| format!("event {}", entry.id))?;
            }
        }

        registry.check_no_conflicting_partitions()?;
        Ok(registry)
    }

    fn add_group(
        &mut self,
        event: EventId,
        event_key: &str,
        entry: &GroupEntry,
        resolves_at_ms: u64,
    ) -> Result<()> {
        let mut ids = Vec::with_capacity(entry.members.len());
        for member in &entry.members {
            let (venue, key, side) = member.resolve()?;
            let contract = self
                .contracts
                .get(venue, &key)
                .context("member was not interned")?;
            // A contract bound to two different events has an ambiguous
            // resolution time, so ranking it would be guesswork.
            if let Some(existing) = self.bindings.get(&contract) {
                ensure!(
                    existing.event == event,
                    "contract {key:?} appears under two events: {:?} and {event_key:?}",
                    self.events[existing.event.0 as usize].key
                );
            } else {
                self.bindings.insert(
                    contract,
                    ContractBinding {
                        contract_id: contract,
                        venue,
                        venue_ticker: key.clone(),
                        event,
                        side,
                        // Only a cross-venue pair carries verification; a single
                        // venue's own listing is the event by definition.
                        verified: entry.verified.unwrap_or(true),
                    },
                );
            }
            ids.push(contract);
        }

        let kind = entry.kind.as_str();
        ensure!(
            kind == "equivalent" || entry.verified.is_none(),
            "group type {kind:?} does not take a verified flag"
        );

        let relation = match kind {
            "complement" => {
                ensure!(ids.len() == 1, "complement takes exactly one member");
                Relation::Complement { contract: ids[0] }
            }
            "exhaustive" => Relation::Exhaustive { members: ids },
            "monotone" => {
                self.check_ladder_order(entry)?;
                Relation::Monotone { ordered: ids }
            }
            "implies" => {
                ensure!(
                    ids.len() == 2,
                    "implies takes exactly two members, antecedent first"
                );
                Relation::Implies {
                    antecedent: ids[0],
                    consequent: ids[1],
                }
            }
            "equivalent" => {
                ensure!(ids.len() == 2, "equivalent takes exactly two members");
                let verified = entry
                    .verified
                    .context("equivalent groups must state verified = true or verified = false")?;
                Relation::Equivalent {
                    a: ids[0],
                    b: ids[1],
                    verified,
                }
            }
            other => bail!(
                "unknown group type {other:?}; expected complement, exhaustive, monotone, implies, or equivalent"
            ),
        };
        self.push_group(relation, resolves_at_ms, false)?;
        Ok(())
    }

    /// A ladder must be written weakest claim first, so its strikes ascend.
    ///
    /// Written the other way round, every adjacent pair looks inverted and the
    /// solver would emit a trade for each one. Checking the order here is the
    /// difference between a config typo and a stream of confident nonsense.
    /// Tickers whose strike cannot be read are left alone rather than guessed at.
    fn check_ladder_order(&self, entry: &GroupEntry) -> Result<()> {
        let mut strikes = Vec::with_capacity(entry.members.len());
        for member in &entry.members {
            let (_, key, _) = member.resolve()?;
            match strike_hundredths(&key) {
                Some(strike) => strikes.push((key, strike)),
                None => {
                    tracing::debug!(ticker = %key, "ladder order unverified: no strike in ticker");
                    return Ok(());
                }
            }
        }
        for pair in strikes.windows(2) {
            ensure!(
                pair[0].1 < pair[1].1,
                "monotone ladder is not ascending by strike: {} then {}",
                pair[0].0,
                pair[1].0
            );
        }
        Ok(())
    }

    /// Two exhaustive sets sharing a contract are almost always a copy-paste
    /// error, and they would double-count that contract's depth across groups.
    fn check_no_conflicting_partitions(&self) -> Result<()> {
        let mut partition_of: HashMap<ContractId, GroupId> = HashMap::new();
        for group in &self.groups {
            if !matches!(group.relation, Relation::Exhaustive { .. }) {
                continue;
            }
            for contract in group.members() {
                if let Some(previous) = partition_of.insert(*contract, group.id) {
                    let ticker = self
                        .bindings
                        .get(contract)
                        .map(|b| b.venue_ticker.as_str())
                        .unwrap_or("?");
                    bail!(
                        "contract {ticker:?} is in two exhaustive sets, groups {} and {}",
                        previous.0,
                        group.id.0
                    );
                }
            }
        }
        Ok(())
    }

    /// Names in the registry that the venue does not list, for one venue.
    ///
    /// Separate from loading because it needs metadata the caller fetched.
    /// A missing ticker is a configuration error, not a runtime one: the engine
    /// would subscribe to a market that does not exist and wait forever for a
    /// snapshot that never arrives.
    pub fn validate_against_markets<S>(&self, venue: Venue, available: &S) -> Vec<&str>
    where
        S: Contains + ?Sized,
    {
        self.tickers(venue)
            .iter()
            .filter(|ticker| !available.contains_ticker(ticker))
            .map(String::as_str)
            .collect()
    }
}

/// Membership test for whatever collection the caller fetched metadata into.
pub trait Contains {
    fn contains_ticker(&self, ticker: &str) -> bool;
}

impl Contains for std::collections::HashSet<String> {
    fn contains_ticker(&self, ticker: &str) -> bool {
        self.contains(ticker)
    }
}

impl Contains for [String] {
    fn contains_ticker(&self, ticker: &str) -> bool {
        self.iter().any(|t| t == ticker)
    }
}

/// Read a strike out of a venue ticker as integer hundredths.
///
/// `KXBTCD-26SEP1417-T73999.99` yields 7399999. Anything that is not a plain
/// decimal after an optional `T` yields `None`, which means "cannot check",
/// not "zero" — a Fed ladder rung like `KXFED-26SEP-C50` has no strike to read.
fn strike_hundredths(ticker: &str) -> Option<i64> {
    let tail = ticker.rsplit('-').next()?;
    let tail = tail.strip_prefix('T').unwrap_or(tail);
    let (whole, frac) = tail.split_once('.').unwrap_or((tail, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut hundredths = whole.parse::<i64>().ok()?.checked_mul(100)?;
    let digit = |i: usize| i64::from(frac.as_bytes().get(i).copied().unwrap_or(b'0') - b'0');
    hundredths = hundredths.checked_add(digit(0) * 10 + digit(1))?;
    Some(hundredths)
}

impl Registry {
    /// Fetch a cached Kalshi universe and build the runtime registry from it.
    ///
    /// This convenience entry point is intentionally equivalent to the startup
    /// path used by the CLI with the default discovery scope.  Callers that
    /// need a series or a custom freshness budget should use
    /// [`Registry::from_discovery_with_scope`].
    pub async fn from_discovery(
        discovery: &crate::discovery::KalshiDiscovery,
        cache: &crate::discovery::DiscoveryCache,
    ) -> Result<Self> {
        Self::from_discovery_with_scope(
            discovery,
            cache,
            &crate::discovery::DiscoveryScope::default(),
            3_600,
        )
        .await
    }

    pub async fn from_discovery_with_scope(
        discovery: &crate::discovery::KalshiDiscovery,
        cache: &crate::discovery::DiscoveryCache,
        scope: &crate::discovery::DiscoveryScope,
        max_age_secs: u64,
    ) -> Result<Self> {
        let clock = crate::clock::WallClock;
        let universe = cache
            .load_or_fetch(discovery, scope, max_age_secs, clock.now_ms())
            .await?;
        Self::from_universe(&universe)
    }

    /// Build the constraint graph from a venue snapshot instead of a file.
    ///
    /// Interning follows the snapshot's sorted ticker order, so two runs that
    /// saw the same venue assign the same [`ContractId`]s — the same discipline
    /// the file loader keeps, for the same reason.
    ///
    /// Every group produced here is marked `inferred`. An event whose close
    /// time the venue did not publish is skipped rather than given a guessed
    /// horizon: ranking would otherwise be arithmetic on a number nobody knows.
    pub fn from_universe(universe: &crate::discovery::Universe) -> Result<Self> {
        use crate::discovery::{event_close_times, infer_groups, to_relation};

        let (inferred, report) = infer_groups(universe);
        let closes = event_close_times(universe);
        let mut registry = Registry::default();

        // Events first, so a group can be bound to one as it is created.  The
        // snapshot is sorted by the live discovery client, but sorting here as
        // well keeps manually constructed/test universes deterministic.
        let mut skipped_no_close = 0usize;
        let mut raw_events: Vec<_> = universe.events.iter().collect();
        raw_events.sort_by(|left, right| left.event_ticker.cmp(&right.event_ticker));
        for raw in raw_events {
            let Some(close) = closes.get(&raw.event_ticker) else {
                continue;
            };
            let Ok(resolves_at) = DateTime::parse_from_rfc3339(close) else {
                skipped_no_close += 1;
                continue;
            };
            let id = EventId(u32::try_from(registry.events.len())?);
            registry.events.push(CanonicalEvent {
                id,
                key: raw.event_ticker.clone(),
                description: raw.title.clone(),
                resolves_at: resolves_at.with_timezone(&Utc),
                resolution_source: format!("kalshi:{}", raw.series_ticker),
                // Series is the natural theme: every market under one series
                // settles off the same source and fails together.
                theme: (!raw.series_ticker.is_empty()).then(|| raw.series_ticker.clone()),
                resolution_rules_hash: 0,
            });
            ensure!(
                registry
                    .event_by_key
                    .insert(raw.event_ticker.clone(), id)
                    .is_none(),
                "duplicate event ticker {:?}",
                raw.event_ticker
            );
        }

        // Intern in ticker order.  Contract ids are positional and this is the
        // invariant that keeps discovery, parser, and book-store ids aligned.
        let mut raw_markets: Vec<_> = universe.markets.iter().collect();
        raw_markets.sort_by(|left, right| left.ticker.cmp(&right.ticker));
        for market in raw_markets {
            if registry
                .contracts
                .get(Venue::Kalshi, &market.ticker)
                .is_some()
            {
                continue;
            }
            let Some(event) = registry.event_by_key.get(&market.event_ticker).copied() else {
                continue;
            };
            let contract = registry.contracts.intern(Venue::Kalshi, &market.ticker)?;
            registry
                .tickers
                .entry(Venue::Kalshi)
                .or_default()
                .push(market.ticker.clone());
            registry.bindings.insert(
                contract,
                ContractBinding {
                    contract_id: contract,
                    venue: Venue::Kalshi,
                    venue_ticker: market.ticker.clone(),
                    event,
                    side: Side::Yes,
                    // A single venue's own listing is the event by definition;
                    // verification is a cross-venue question.
                    verified: true,
                },
            );
        }

        let mut skipped_unbound = 0usize;
        for group in &inferred {
            let Some(relation) = to_relation(&registry.contracts, group) else {
                skipped_unbound += 1;
                continue;
            };
            let Some(event) = registry.event_by_key.get(&group.event_ticker).copied() else {
                skipped_unbound += 1;
                continue;
            };
            let resolves_at_ms = registry
                .event(event)
                .map(|e| u64::try_from(e.resolves_at.timestamp_millis()).unwrap_or(0))
                .unwrap_or(0);
            if registry.push_group(relation, resolves_at_ms, true).is_err() {
                skipped_unbound += 1;
            }
        }

        tracing::info!(
            markets = report.markets,
            events = report.events,
            complements = report.complements,
            exhaustive = report.exhaustive_sets,
            ladders = report.ladders,
            not_inferable = report.events_not_inferable,
            skipped_unbound,
            skipped_no_close,
            groups = registry.groups.len(),
            "registry inferred from venue metadata"
        );
        registry.check_no_conflicting_partitions()?;
        Ok(registry)
    }

    /// Groups nobody has confirmed by hand.
    pub fn inferred_count(&self) -> usize {
        self.groups.iter().filter(|g| g.inferred).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u32) -> ContractId {
        ContractId(id)
    }

    #[test]
    fn reverse_index_finds_every_group_a_contract_belongs_to() {
        let registry = Registry::new([
            (
                Relation::Exhaustive {
                    members: vec![c(0), c(1), c(2)],
                },
                1_000,
            ),
            (Relation::Complement { contract: c(1) }, 1_000),
        ])
        .unwrap();
        assert_eq!(registry.groups_for(c(1)), &[GroupId(0), GroupId(1)]);
        assert_eq!(registry.groups_for(c(2)), &[GroupId(0)]);
        // A contract nobody constrains is not an error, it just has no groups.
        assert!(registry.groups_for(c(9)).is_empty());
    }

    #[test]
    fn malformed_groups_are_rejected_at_construction() {
        assert_eq!(
            Registry::new([(
                Relation::Exhaustive {
                    members: vec![c(0)]
                },
                1
            )])
            .err(),
            Some(RegistryError::TooFewMembers {
                group: GroupId(0),
                got: 1
            })
        );
        assert_eq!(
            Registry::new([(
                Relation::Exhaustive {
                    members: vec![c(0), c(1), c(0)]
                },
                1
            )])
            .err(),
            Some(RegistryError::DuplicateMember {
                group: GroupId(0),
                contract: c(0)
            })
        );
    }

    const MINIMAL: &str = r#"
[[event]]
id = "fed-2026-09"
description = "FOMC rate decision, September 2026"
resolves_at = "2026-09-17T18:00:00Z"
resolution_source = "FOMC statement"

[[event.group]]
type = "exhaustive"
members = [
  { venue = "kalshi", ticker = "KXFED-26SEP-C50" },
  { venue = "kalshi", ticker = "KXFED-26SEP-C25" },
  { venue = "kalshi", ticker = "KXFED-26SEP-NC" },
]

[[event.group]]
type = "equivalent"
verified = false
members = [
  { venue = "kalshi", ticker = "KXFED-26SEP-C25" },
  { venue = "polymarket", token = "0xabc" },
]
"#;

    #[test]
    fn loads_events_bindings_groups_and_the_index_from_toml() {
        let registry = Registry::parse(MINIMAL).unwrap();
        let event = registry.event_by_key("fed-2026-09").unwrap();
        assert_eq!(event.resolution_source, "FOMC statement");
        assert_eq!(event.resolves_at.to_rfc3339(), "2026-09-17T18:00:00+00:00");
        assert_eq!(registry.groups().len(), 2);

        // Every Kalshi ticker interns before the Polymarket token, in file order.
        assert_eq!(
            registry.tickers(Venue::Kalshi),
            ["KXFED-26SEP-C50", "KXFED-26SEP-C25", "KXFED-26SEP-NC"]
        );
        assert_eq!(registry.tickers(Venue::Polymarket), ["0xabc"]);
        let c25 = registry
            .contracts()
            .get(Venue::Kalshi, "KXFED-26SEP-C25")
            .unwrap();
        assert_eq!(c25, ContractId(1));
        assert_eq!(
            registry.contracts().get(Venue::Polymarket, "0xabc"),
            Some(ContractId(3))
        );

        // The reverse index puts the shared contract in both of its groups.
        assert_eq!(registry.groups_for(c25), &[GroupId(0), GroupId(1)]);
        let binding = registry.binding(c25).unwrap();
        assert_eq!(binding.venue, Venue::Kalshi);
        assert_eq!(binding.event, event.id);
        assert_eq!(binding.side, Side::Yes);

        // Resolution time reaches the group, which is what ranking needs.
        let group = registry.group(GroupId(0)).unwrap();
        assert_eq!(group.resolves_at_ms, 1_789_668_000_000);
        assert!(matches!(
            registry.group(GroupId(1)).unwrap().relation,
            Relation::Equivalent {
                verified: false,
                ..
            }
        ));
    }

    #[test]
    fn a_ladder_written_backwards_is_rejected() {
        let ladder = |a: &str, b: &str| {
            format!(
                r#"
[[event]]
id = "btc"
description = "BTC"
resolves_at = "2026-09-14T21:00:00Z"
resolution_source = "Kalshi"

[[event.group]]
type = "monotone"
members = [
  {{ venue = "kalshi", ticker = "{a}" }},
  {{ venue = "kalshi", ticker = "{b}" }},
]
"#
            )
        };
        // Ascending strike is weakest claim first, which is the required order.
        assert!(
            Registry::parse(&ladder(
                "KXBTCD-26SEP1417-T73999.99",
                "KXBTCD-26SEP1417-T74249.99"
            ))
            .is_ok()
        );
        // Reversed, every adjacent pair would look inverted and the solver would
        // emit a trade for each one.
        let error = Registry::parse(&ladder(
            "KXBTCD-26SEP1417-T74249.99",
            "KXBTCD-26SEP1417-T73999.99",
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains("not ascending"), "{error:#}");
        // A ticker with no strike to read is left alone rather than guessed at.
        assert!(Registry::parse(&ladder("KXFED-26SEP-C50", "KXFED-26SEP-C25")).is_ok());
    }

    #[test]
    fn malformed_files_fail_at_load_with_a_reason() {
        let cases = [
            (
                "verifed = true",
                "[[event.group]]
type = \"equivalent\"
verifed = true
members = []",
            ),
            (
                "does not take a verified flag",
                "[[event.group]]
type = \"exhaustive\"
verified = true
members = [{ venue = \"kalshi\", ticker = \"A\" }, { venue = \"kalshi\", ticker = \"B\" }]",
            ),
            (
                "must state verified",
                "[[event.group]]
type = \"equivalent\"
members = [{ venue = \"kalshi\", ticker = \"A\" }, { venue = \"polymarket\", token = \"0x1\" }]",
            ),
            (
                "unknown group type",
                "[[event.group]]
type = \"exhastive\"
members = [{ venue = \"kalshi\", ticker = \"A\" }, { venue = \"kalshi\", ticker = \"B\" }]",
            ),
            (
                "unknown venue",
                "[[event.group]]
type = \"complement\"
members = [{ venue = \"betfair\", ticker = \"A\" }]",
            ),
            (
                "neither ticker nor token",
                "[[event.group]]
type = \"complement\"
members = [{ venue = \"kalshi\" }]",
            ),
            ("is not RFC3339", ""),
        ];
        for (needle, group) in cases {
            let resolves_at = if needle == "is not RFC3339" {
                "next tuesday"
            } else {
                "2026-09-17T18:00:00Z"
            };
            let text = format!(
                "[[event]]
id = \"e\"
description = \"d\"
resolves_at = \"{resolves_at}\"
resolution_source = \"s\"

{group}"
            );
            let error = Registry::parse(&text)
                .expect_err(&format!("expected a failure mentioning {needle:?}"));
            assert!(
                format!("{error:#}").contains(needle),
                "expected {needle:?} in: {error:#}"
            );
        }
        // An empty file is a mistake, not an empty engine.
        assert!(Registry::parse("").is_err());
    }

    #[test]
    fn a_contract_cannot_sit_in_two_partitions_or_two_events() {
        let two_sets = r#"
[[event]]
id = "e"
description = "d"
resolves_at = "2026-09-17T18:00:00Z"
resolution_source = "s"

[[event.group]]
type = "exhaustive"
members = [{ venue = "kalshi", ticker = "A" }, { venue = "kalshi", ticker = "B" }]

[[event.group]]
type = "exhaustive"
members = [{ venue = "kalshi", ticker = "B" }, { venue = "kalshi", ticker = "C" }]
"#;
        let error = Registry::parse(two_sets).unwrap_err();
        assert!(
            format!("{error:#}").contains("two exhaustive sets"),
            "{error:#}"
        );

        let two_events = r#"
[[event]]
id = "first"
description = "d"
resolves_at = "2026-09-17T18:00:00Z"
resolution_source = "s"

[[event.group]]
type = "complement"
members = [{ venue = "kalshi", ticker = "A" }]

[[event]]
id = "second"
description = "d"
resolves_at = "2026-10-17T18:00:00Z"
resolution_source = "s"

[[event.group]]
type = "complement"
members = [{ venue = "kalshi", ticker = "A" }]
"#;
        // Two resolution times for one contract means ranking it is guesswork.
        let error = Registry::parse(two_events).unwrap_err();
        assert!(format!("{error:#}").contains("two events"), "{error:#}");
    }

    #[test]
    fn missing_tickers_are_reported_against_venue_metadata() {
        let registry = Registry::parse(MINIMAL).unwrap();
        let listed: std::collections::HashSet<String> = ["KXFED-26SEP-C50", "KXFED-26SEP-NC"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            registry.validate_against_markets(Venue::Kalshi, &listed),
            vec!["KXFED-26SEP-C25"]
        );
        // The Polymarket token is not checked against a Kalshi listing.
        assert!(
            registry
                .validate_against_markets(Venue::Polymarket, &listed)
                .len()
                == 1
        );
    }

    #[test]
    fn strikes_parse_out_of_real_tickers() {
        assert_eq!(
            strike_hundredths("KXBTCD-26SEP1417-T73999.99"),
            Some(7_399_999)
        );
        assert_eq!(
            strike_hundredths("KXBTCD-26SEP1417-T74000"),
            Some(7_400_000)
        );
        assert_eq!(strike_hundredths("KXFED-26SEP-C50"), None);
        assert_eq!(strike_hundredths(""), None);
    }

    #[test]
    fn the_shipped_registry_loads() {
        let registry = Registry::from_toml(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/registry.toml"),
        )
        .unwrap();
        assert_eq!(registry.tickers(Venue::Kalshi).len(), 80);
        // 80 complement groups plus the ladder over all of them.
        assert_eq!(registry.groups().len(), 81);
        assert_eq!(registry.events().len(), 1);
        // Every strike is in its own complement group and in the ladder.
        for id in 0..80u32 {
            assert_eq!(registry.groups_for(ContractId(id)).len(), 2);
        }
    }

    #[test]
    fn ladder_states_are_exactly_the_cut_points() {
        let states = Relation::Monotone {
            ordered: vec![c(0), c(1), c(2)],
        }
        .resolution_states();
        assert_eq!(
            states,
            vec![vec![], vec![c(0)], vec![c(0), c(1)], vec![c(0), c(1), c(2)],]
        );
        // A two-rung ladder and the implication it encodes agree exactly.
        let ladder = Relation::Monotone {
            ordered: vec![c(0), c(1)],
        }
        .resolution_states();
        let implies = Relation::Implies {
            antecedent: c(1),
            consequent: c(0),
        }
        .resolution_states();
        assert_eq!(ladder, implies);
    }
}
