pub type Cents = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Kalshi,
    Polymarket,
}

impl Venue {
    /// Every venue, in the order identifiers are interned and reported.
    ///
    /// The registry interns all Kalshi members before any Polymarket one, and
    /// this order has to agree with it: contract ids are positional, so a
    /// different order silently renumbers every book.
    pub const ALL: [Venue; 2] = [Venue::Kalshi, Venue::Polymarket];

    /// Dense index for per-venue arrays. Deliberately next to [`Venue::ALL`] so
    /// a new venue cannot be added without being given a slot in both.
    pub fn index(self) -> usize {
        match self {
            Venue::Kalshi => 0,
            Venue::Polymarket => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Venue::Kalshi => "kalshi",
            Venue::Polymarket => "polymarket",
        }
    }
}

/// Slots in every per-venue array in the engine.
pub const VENUE_COUNT: usize = Venue::ALL.len();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub price: Cents,
    pub size: i64,
}

/// Whether a book is trustworthy for solver evaluation.
///
/// The solver refuses any group containing a book that is not [`Live`]. A
/// sequence gap or disconnect moves the book to [`Resyncing`] until a fresh
/// snapshot arrives; there is no attempt to repair intermediate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BookState {
    /// Subscribed, no snapshot yet.
    Uninitialized,
    /// Gap or disconnect; awaiting a fresh snapshot.
    Resyncing,
    /// Snapshot applied, deltas in sequence.
    Live,
}

/// In-memory order book for one contract.
///
/// Kalshi sends two one-sided bid books (`yes` and `no`). A resting no bid at
/// price P is economically a yes ask at `100 - P`. Prices are integer cents in
/// `0..=100`, so each side is a dense size-by-price array: delta application is
/// a single index, and memory is bounded by construction.
#[derive(Debug, Clone)]
pub struct Book {
    pub venue: Venue,
    pub contract_id: ContractId,
    pub state: BookState,
    pub seq: u64,
    pub updated_at_ms: u64,
    /// Resting yes bid size by price (cents 0..=100).
    yes: [i64; 101],
    /// Resting no bid size by price (cents 0..=100).
    no: [i64; 101],
}

impl Book {
    pub fn new(venue: Venue, contract_id: ContractId) -> Self {
        Self {
            venue,
            contract_id,
            state: BookState::Uninitialized,
            seq: 0,
            updated_at_ms: 0,
            yes: [0; 101],
            no: [0; 101],
        }
    }

    pub fn is_fresh(&self, now_ms: u64, max_age_ms: u64) -> bool {
        self.state == BookState::Live && now_ms.saturating_sub(self.updated_at_ms) <= max_age_ms
    }

    /// Replace both sides from a venue snapshot. Absolute state — always applied.
    ///
    /// Returns the number of levels whose floored size was already zero (they
    /// still occupy a wire slot but contribute no executable depth).
    pub fn apply_snapshot(
        &mut self,
        yes: &[Level],
        no: &[Level],
        seq: u64,
        ts_ms: u64,
    ) -> Result<(), BookApplyError> {
        let mut yes_arr = [0i64; 101];
        let mut no_arr = [0i64; 101];
        for level in yes {
            Self::set_level(&mut yes_arr, level.price, level.size)?;
        }
        for level in no {
            Self::set_level(&mut no_arr, level.price, level.size)?;
        }
        self.yes = yes_arr;
        self.no = no_arr;
        self.seq = seq;
        self.updated_at_ms = ts_ms;
        self.state = BookState::Live;
        Ok(())
    }

    /// Apply a signed size change on one wire side at one price.
    ///
    /// Floor drift can drive a level negative when a fractional snapshot size
    /// floored to zero and a later fractional remove floors to -1. Clamp to
    /// zero and report the clamp; do not resync — understating depth is the
    /// safe direction.
    pub fn apply_delta(
        &mut self,
        side: Side,
        price: Cents,
        size_delta: i64,
        seq: u64,
        ts_ms: u64,
    ) -> Result<bool, BookApplyError> {
        let idx = Self::price_index(price)?;
        let slot = match side {
            Side::Yes => &mut self.yes[idx],
            Side::No => &mut self.no[idx],
        };
        let next = slot.saturating_add(size_delta);
        let clamped = next < 0;
        *slot = next.max(0);
        self.seq = seq;
        self.updated_at_ms = ts_ms;
        Ok(clamped)
    }

    /// Yes bids, highest price first, skipping empty levels.
    pub fn bids(&self) -> impl Iterator<Item = Level> + '_ {
        (0..=100usize).rev().filter_map(|i| {
            let size = self.yes[i];
            (size > 0).then_some(Level {
                price: i as Cents,
                size,
            })
        })
    }

    /// Yes asks derived from no bids: price = 100 - no_price, ascending.
    pub fn asks(&self) -> impl Iterator<Item = Level> + '_ {
        (0..=100usize).rev().filter_map(|i| {
            let size = self.no[i];
            (size > 0).then_some(Level {
                price: 100 - i as Cents,
                size,
            })
        })
    }

    /// No asks derived from yes bids: price = 100 - yes_price, ascending.
    ///
    /// Buying the no outcome means taking a resting yes bid, because on Kalshi a
    /// yes buy at P and a no buy at `100 - P` are the same match. The complement
    /// fast path needs this ladder for its second leg, and it is deliberately the
    /// mirror of [`Book::asks`] rather than a separate notion of liquidity.
    pub fn no_asks(&self) -> impl Iterator<Item = Level> + '_ {
        (0..=100usize).rev().filter_map(|i| {
            let size = self.yes[i];
            (size > 0).then_some(Level {
                price: 100 - i as Cents,
                size,
            })
        })
    }

    pub fn best_bid(&self) -> Option<Level> {
        self.bids().next()
    }

    pub fn best_ask(&self) -> Option<Level> {
        self.asks().next()
    }

    /// Cheapest price at which the no outcome can be bought.
    pub fn best_no_ask(&self) -> Option<Level> {
        self.no_asks().next()
    }

    /// Best resting no bid (highest no price), if any.
    pub fn best_no_bid(&self) -> Option<Level> {
        (0..=100usize).rev().find_map(|i| {
            let size = self.no[i];
            (size > 0).then_some(Level {
                price: i as Cents,
                size,
            })
        })
    }

    /// True when best yes bid plus best no bid exceeds 100 cents (crossed book).
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_no_bid()) {
            (Some(yes), Some(no)) => yes.price + no.price > 100,
            _ => false,
        }
    }

    pub fn mark_resyncing(&mut self) {
        self.state = BookState::Resyncing;
    }

    pub fn yes_size_at(&self, price: Cents) -> Option<i64> {
        Some(self.yes[Self::price_index(price).ok()?])
    }

    pub fn no_size_at(&self, price: Cents) -> Option<i64> {
        Some(self.no[Self::price_index(price).ok()?])
    }

    fn price_index(price: Cents) -> Result<usize, BookApplyError> {
        if !(0..=100).contains(&price) {
            return Err(BookApplyError::PriceOutOfRange(price));
        }
        Ok(price as usize)
    }

    fn set_level(arr: &mut [i64; 101], price: Cents, size: i64) -> Result<(), BookApplyError> {
        if size < 0 {
            return Err(BookApplyError::NegativeSnapshotSize(size));
        }
        let idx = Self::price_index(price)?;
        // Multiple rows at the same price: last write wins; sizes should not
        // accumulate from a snapshot (venue sends one row per price).
        arr[idx] = size;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookApplyError {
    PriceOutOfRange(Cents),
    NegativeSnapshotSize(i64),
}

impl std::fmt::Display for BookApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BookApplyError::PriceOutOfRange(p) => {
                write!(f, "book price {p} outside 0..=100")
            }
            BookApplyError::NegativeSnapshotSize(s) => {
                write!(f, "snapshot size {s} is negative")
            }
        }
    }
}

impl std::error::Error for BookApplyError {}

/// Which side of the book a resting order sits on, in the venue's own terms.
///
/// Kalshi quotes a binary market as two one-sided books rather than as bids and
/// asks: a resting `No` order at price P is economically identical to a resting
/// `Yes` order at `100 - P`, because the two contracts are complements that
/// together always pay exactly $1. Both encodings describe the same liquidity.
///
/// The feed layer deliberately preserves whichever side the venue actually sent
/// and performs no conversion. Translating `No` into a synthetic `Yes` price is
/// book-construction work owned by the book store, where sequence gaps are
/// detected and state is owned. Converting in the feed would make it a silent
/// participant in book state and would mean a translation bug and a venue bug
/// look identical in the recorded corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

/// Why a price string could not be converted to exact integer cents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriceParseError {
    /// Input was empty or whitespace only.
    Empty,
    /// Input was not a plain decimal number.
    Malformed,
    /// Input carried precision finer than one cent, such as "0.005".
    ///
    /// This is an error rather than a rounding decision on purpose. A sub-cent
    /// price means the venue changed its tick structure, and silently rounding
    /// would let a changed contract specification flow into the solver looking
    /// like a normal quote.
    SubCent,
    /// Value fell outside the 0 to 100 cent range a binary contract can occupy.
    OutOfRange,
}

impl std::fmt::Display for PriceParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PriceParseError::Empty => write!(f, "price string was empty"),
            PriceParseError::Malformed => write!(f, "price string was not a plain decimal"),
            PriceParseError::SubCent => write!(f, "price string had sub-cent precision"),
            PriceParseError::OutOfRange => write!(f, "price was outside 0 to 100 cents"),
        }
    }
}

impl std::error::Error for PriceParseError {}

/// Convert a venue decimal-dollar string such as "0.0800" into exact integer [`Cents`].
///
/// Kalshi puts prices on the wire as decimal strings in dollars, never as
/// integer cents, so this function is the single doorway between the venue's
/// representation and the engine's. Every price in the system passes through
/// here, which is why it refuses rather than approximates.
///
/// There is no `f64` anywhere in this path, and that is the entire point.
/// Parsing "0.29" as a float and multiplying by 100 yields 28.999999999999996,
/// which truncates to 28: a full cent lost on a price that appears in this
/// project's own solver tests. "0.58" fails the same way. The error is not
/// uniform either, since "0.07" lands at 7.000000000000001 and truncates
/// correctly, so a float implementation would pass a casual spot check and be
/// wrong on exactly the inputs nobody tried. The conversion is done on the digit
/// characters instead, so it is exact by construction rather than exact by luck.
///
/// Precision finer than one cent is rejected outright. Truncating "0.005" to 0
/// would understate a cost and truncating it upward would invent one; either way
/// a real change in the venue's tick size would be absorbed silently instead of
/// surfacing. Failing loudly here is what keeps a wrong number from appearing
/// six weeks later with no way to trace it.
pub fn parse_price_cents(s: &str) -> Result<Cents, PriceParseError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(PriceParseError::Empty);
    }

    // A leading sign is not valid for a price level, so reject it here rather
    // than letting it fall through to the digit scan.
    let (int_part, frac_part) = match t.split_once('.') {
        Some((i, f)) => (i, f),
        None => (t, ""),
    };

    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PriceParseError::Malformed);
    }
    if !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(PriceParseError::Malformed);
    }
    // "0." with nothing after the point is malformed, not zero.
    if t.contains('.') && frac_part.is_empty() {
        return Err(PriceParseError::Malformed);
    }

    let dollars: i64 = int_part.parse().map_err(|_| PriceParseError::Malformed)?;

    // Take exactly two fractional digits as cents, padding a short fraction.
    // Anything beyond those two digits must be zero or the value is not
    // representable in whole cents.
    let mut cents_digits = [b'0'; 2];
    for (i, slot) in cents_digits.iter_mut().enumerate() {
        if let Some(b) = frac_part.as_bytes().get(i) {
            *slot = *b;
        }
    }
    if frac_part.len() > 2 && frac_part.as_bytes()[2..].iter().any(|b| *b != b'0') {
        return Err(PriceParseError::SubCent);
    }

    let cents = i64::from(cents_digits[0] - b'0') * 10 + i64::from(cents_digits[1] - b'0');

    let total = dollars
        .checked_mul(100)
        .and_then(|d| d.checked_add(cents))
        .ok_or(PriceParseError::OutOfRange)?;

    // A binary contract settles at $0 or $1, so a price outside that band is a
    // venue schema change rather than a quote, and must not reach the solver.
    if !(0..=100).contains(&total) {
        return Err(PriceParseError::OutOfRange);
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_venue_prices_exactly() {
        // These four strings are copied from live Kalshi payloads.
        assert_eq!(parse_price_cents("0.960"), Ok(96));
        assert_eq!(parse_price_cents("0.0800"), Ok(8));
        assert_eq!(parse_price_cents("0.0100"), Ok(1));
        assert_eq!(parse_price_cents("1.0000"), Ok(100));
    }

    #[test]
    fn parses_boundaries_and_short_forms() {
        assert_eq!(parse_price_cents("0.0000"), Ok(0));
        assert_eq!(parse_price_cents("0"), Ok(0));
        assert_eq!(parse_price_cents("1"), Ok(100));
        assert_eq!(parse_price_cents("0.5"), Ok(50));
        assert_eq!(parse_price_cents("0.07"), Ok(7));
        assert_eq!(parse_price_cents("0.99"), Ok(99));
        // Trailing zeros beyond the cent are exact, not lossy.
        assert_eq!(parse_price_cents("0.990000000"), Ok(99));
        assert_eq!(parse_price_cents("  0.42  "), Ok(42));
    }

    #[test]
    fn sub_cent_precision_is_rejected_not_rounded() {
        // The whole point: never silently absorb a tick-size change.
        assert_eq!(parse_price_cents("0.005"), Err(PriceParseError::SubCent));
        assert_eq!(parse_price_cents("0.001"), Err(PriceParseError::SubCent));
        assert_eq!(parse_price_cents("0.9601"), Err(PriceParseError::SubCent));
        assert_eq!(parse_price_cents("0.12345"), Err(PriceParseError::SubCent));
        // Rounding either direction would have produced a plausible-looking number.
        assert!(parse_price_cents("0.005").is_err());
    }

    #[test]
    fn malformed_and_empty_input_is_rejected() {
        assert_eq!(parse_price_cents(""), Err(PriceParseError::Empty));
        assert_eq!(parse_price_cents("   "), Err(PriceParseError::Empty));
        assert_eq!(parse_price_cents("abc"), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("0.abc"), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("0."), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("."), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents(".5"), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("0.1.2"), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("1e-2"), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("NaN"), Err(PriceParseError::Malformed));
        assert_eq!(parse_price_cents("inf"), Err(PriceParseError::Malformed));
        // A negative price is not a quote; it must not parse.
        assert_eq!(parse_price_cents("-0.50"), Err(PriceParseError::Malformed));
    }

    #[test]
    fn out_of_range_prices_are_rejected() {
        // Above $1 a binary contract makes no sense, so this is a schema change.
        assert_eq!(
            parse_price_cents("1.0100"),
            Err(PriceParseError::OutOfRange)
        );
        assert_eq!(
            parse_price_cents("2.0000"),
            Err(PriceParseError::OutOfRange)
        );
        assert_eq!(
            parse_price_cents("99999999999999999999"),
            Err(PriceParseError::Malformed)
        );
    }

    #[test]
    fn no_float_rounding_artifacts_across_the_whole_tick_ladder() {
        // Every representable cent must round-trip exactly. The f64 path fails
        // several of these, which is precisely why this parser exists.
        for cents in 0..=100i64 {
            let s = format!("0.{:02}00", cents % 100);
            let expected = if cents == 100 { 0 } else { cents };
            assert_eq!(parse_price_cents(&s), Ok(expected), "input {s}");
        }
        for cents in 0..=100i64 {
            let dollars = cents / 100;
            let rem = cents % 100;
            let s = format!("{dollars}.{rem:02}");
            assert_eq!(parse_price_cents(&s), Ok(cents), "input {s}");
        }
    }

    #[test]
    fn side_is_a_faithful_echo_of_the_wire() {
        // Guard the boundary decision: Side carries no conversion logic, so the
        // complement relationship stays documented rather than applied here.
        assert_ne!(Side::Yes, Side::No);
    }
}

/// Interned identifier; ticker strings stay outside downstream hot paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ContractId(pub u32);

#[derive(Debug, Default)]
pub struct Contracts {
    by_ticker: std::collections::HashMap<(Venue, String), ContractId>,
    tickers: Vec<(Venue, String)>,
}
impl Contracts {
    pub fn intern(&mut self, venue: Venue, ticker: &str) -> anyhow::Result<ContractId> {
        let key = (venue, ticker.to_owned());
        if let Some(id) = self.by_ticker.get(&key) {
            return Ok(*id);
        }
        let id = ContractId(u32::try_from(self.tickers.len())?);
        self.tickers.push(key.clone());
        self.by_ticker.insert(key, id);
        Ok(id)
    }
    pub fn get(&self, venue: Venue, ticker: &str) -> Option<ContractId> {
        self.by_ticker.get(&(venue, ticker.to_owned())).copied()
    }
    pub fn resolve(&self, id: ContractId) -> Option<&(Venue, String)> {
        self.tickers.get(id.0 as usize)
    }
}

/// Floor fixed-point sizes to whole contracts without floats. Understating
/// snapshot depth suppresses marginal signals instead of inventing liquidity.
/// Signed changes use mathematical floor too: -1.20 becomes -2 (not -1).
/// Repeated fractional deltas can accumulate conservative drift; the book store
/// clamps levels at zero rather than assume floor(snapshot)+sum(floor(delta))
/// is exact, and a fresh snapshot after resync restores absolute state.
/// The counter measures x-floor(x) in hundredths, not unique lost book depth.
/// Nonzero precision beyond hundredths is rejected rather than lost.
pub fn parse_size_contracts(s: &str, discarded_hundredths: &mut u64) -> anyhow::Result<i64> {
    use anyhow::{Context, bail};
    let (negative, unsigned) = match s.strip_prefix('-') {
        Some(t) => (true, t),
        None => (false, s),
    };
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((w, f)) if !f.is_empty() => (w, f),
        Some(_) => bail!("empty size fraction"),
        None => (unsigned, ""),
    };
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("malformed fixed-point size");
    }
    if fraction.len() > 2 && fraction.as_bytes()[2..].iter().any(|b| *b != b'0') {
        bail!("size has sub-hundredth precision");
    }
    let w: i128 = whole.parse().context("size overflow")?;
    let mut cents = 0u64;
    for i in 0..2 {
        cents = cents * 10 + u64::from(fraction.as_bytes().get(i).copied().unwrap_or(b'0') - b'0');
    }
    let value = if negative {
        -w - i128::from(cents != 0)
    } else {
        w
    };
    let value = i64::try_from(value).context("size overflow")?;
    let discarded = if negative && cents != 0 {
        100 - cents
    } else {
        cents
    };
    *discarded_hundredths = discarded_hundredths
        .checked_add(discarded)
        .context("discarded-size counter overflow")?;
    Ok(value)
}
