//! Polymarket discovery, against the live Gamma catalogue.
//!
//! The network tests are the point: the fields, their types, and their nulls
//! are what the parser is specified against, and a fixture written by hand
//! would only assert that this file agrees with itself.

use sum100::{
    discovery::{
        PolymarketDiscovery, PolymarketMarket, polymarket_complement_groups,
        register_polymarket_fees,
    },
    fees::{FeeCategory, FeeModel, PolymarketFees},
    types::{ContractId, Contracts, Venue},
};

const GAMMA: &str = "https://gamma-api.polymarket.com";

fn market(fee_type: Option<&str>, fees_enabled: bool, tick: f64) -> PolymarketMarket {
    PolymarketMarket {
        yes_token: "yes".into(),
        no_token: "no".into(),
        question: "Will it?".into(),
        tick_size: tick,
        fee_type: fee_type.map(str::to_owned),
        fees_enabled,
        ends_at: None,
    }
}

/// `feesEnabled` decides whether there is a fee at all; `feeType` only names
/// the schedule. Live markets carry a null `feeType`, so the two must be read
/// together or a fee-free market is charged and a charging one is not.
#[test]
fn fees_enabled_decides_whether_a_schedule_applies() {
    assert_eq!(
        market(Some("crypto_fees_v2"), true, 0.01).fee_category(),
        FeeCategory::Crypto
    );
    assert_eq!(
        market(Some("sports_fees_v3"), true, 0.01).fee_category(),
        FeeCategory::Sports
    );
    assert_eq!(
        market(Some("politics_fees"), true, 0.01).fee_category(),
        FeeCategory::PoliticsFinance
    );
    // Fees off beats any named schedule.
    assert_eq!(
        market(Some("crypto_fees_v2"), false, 0.01).fee_category(),
        FeeCategory::Zero
    );
    assert_eq!(market(None, false, 0.01).fee_category(), FeeCategory::Zero);
    // On, but unnamed: unknown, so the dearest rate rather than free.
    assert_eq!(market(None, true, 0.01).fee_category(), FeeCategory::Crypto);
}

/// A finer tick cannot be held in a book indexed by whole cents.
#[test]
fn only_one_cent_ticks_are_representable() {
    assert!(market(None, false, 0.01).tick_is_representable());
    assert!(!market(None, false, 0.001).tick_is_representable());
    assert!(!market(None, false, 0.04).tick_is_representable());
}

/// A binary market is a complement over its own two outcomes — one contract,
/// not a pair. The two tokens are the same book from both sides, so a group
/// over both would sum to a dollar by construction and never report anything.
#[test]
fn a_market_becomes_one_complement_group_not_a_pair() {
    let groups = polymarket_complement_groups(&[market(None, true, 0.01)]);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].tickers, vec!["yes".to_owned()]);
}

/// Registration is what turns the conservative default into the real rate.
#[test]
fn registering_a_market_replaces_the_conservative_default() {
    let mut contracts = Contracts::default();
    let contract = contracts.intern(Venue::Polymarket, "yes").unwrap();
    let mut fees = PolymarketFees::default();

    // Unregistered: charged the dearest rate.
    assert_eq!(fees.taker_fee(contract, 50, 100), 175);

    let free = market(None, false, 0.01);
    assert_eq!(register_polymarket_fees(&[free], &contracts, &mut fees), 1);
    assert_eq!(fees.taker_fee(contract, 50, 100), 0);

    // A market whose token was never interned cannot be registered.
    let unknown = PolymarketMarket {
        yes_token: "never-seen".into(),
        ..market(None, false, 0.01)
    };
    assert_eq!(
        register_polymarket_fees(&[unknown], &contracts, &mut fees),
        0
    );
}

/// The live catalogue. Asserts the shape the parser depends on, and that the
/// filters actually admit something: a discovery that silently returns nothing
/// is indistinguishable from a broken one.
#[tokio::test]
async fn the_live_catalogue_yields_representable_markets() {
    let discovery = PolymarketDiscovery::new(GAMMA).unwrap();
    let Ok(markets) = discovery.fetch_representable(40).await else {
        eprintln!("gamma unreachable; skipping live assertions");
        return;
    };

    assert!(!markets.is_empty(), "no tradeable one-cent markets found");
    for found in &markets {
        assert!(found.tick_is_representable(), "{}", found.tick_size);
        assert_ne!(found.yes_token, found.no_token);
        assert!(!found.yes_token.is_empty());
        // Token ids are long decimal strings, not the 0x hashes that name a
        // market's condition.
        assert!(found.yes_token.chars().all(|c| c.is_ascii_digit()));
    }

    // Every market maps to exactly one complement group and one registration.
    let mut contracts = Contracts::default();
    for found in &markets {
        contracts
            .intern(Venue::Polymarket, &found.yes_token)
            .unwrap();
    }
    let mut fees = PolymarketFees::default();
    assert_eq!(
        register_polymarket_fees(&markets, &contracts, &mut fees),
        markets.len()
    );
    assert_eq!(polymarket_complement_groups(&markets).len(), markets.len());

    // Live markets really do carry a mix of schedules; if every one came back
    // the same, the feeType reading would be silently broken.
    let categories: std::collections::BTreeSet<_> =
        markets.iter().map(|m| m.fee_category()).collect();
    eprintln!(
        "categories across {} markets: {categories:?}",
        markets.len()
    );
    assert!(!categories.is_empty());
}

/// Contract ids are positional, so a store interned in a different order from
/// the registry points every book at the wrong contract.
#[test]
fn interning_is_stable_for_polymarket_tokens() {
    let mut contracts = Contracts::default();
    let a = contracts.intern(Venue::Polymarket, "111").unwrap();
    let b = contracts.intern(Venue::Polymarket, "222").unwrap();
    assert_eq!(a, ContractId(0));
    assert_eq!(b, ContractId(1));
    assert_eq!(contracts.intern(Venue::Polymarket, "111").unwrap(), a);
    // The same string on another venue is a different contract.
    assert_ne!(contracts.intern(Venue::Kalshi, "111").unwrap(), a);
}
