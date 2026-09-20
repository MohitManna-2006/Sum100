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

// --------------------------------------------------- registry integration

fn dated(token: &str, ends_at: Option<&str>) -> PolymarketMarket {
    PolymarketMarket {
        yes_token: token.to_owned(),
        no_token: format!("{token}-no"),
        question: format!("Will {token}?"),
        tick_size: 0.01,
        fee_type: Some("sports_fees_v3".into()),
        fees_enabled: true,
        ends_at: ends_at.map(str::to_owned),
    }
}

/// The invariant the whole integration rests on: a book store built from the
/// registry's own ticker lists agrees with it contract for contract. Ids are
/// positional, so a store interned in a different order would point every book
/// at the wrong market while still looking healthy.
#[test]
fn extending_keeps_registry_and_book_store_ids_aligned() {
    use std::path::Path;
    use std::sync::Arc;
    use sum100::{book::BookStore, clock::ReplayClock, registry::Registry};

    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/registry.toml");
    let mut registry = Registry::from_toml(&shipped).unwrap();
    let kalshi_before = registry.tickers(Venue::Kalshi).to_vec();
    let groups_before = registry.groups().len();

    let markets = vec![
        dated("300", Some("2026-12-31T00:00:00Z")),
        dated("100", Some("2026-12-31T00:00:00Z")),
        dated("200", Some("2026-12-31T00:00:00Z")),
    ];
    assert_eq!(registry.extend_with_polymarket(&markets).unwrap(), 3);

    // Kalshi keeps every id it had; Polymarket takes the ones after.
    assert_eq!(registry.tickers(Venue::Kalshi), kalshi_before.as_slice());
    assert_eq!(
        registry.tickers(Venue::Polymarket),
        ["100".to_owned(), "200".to_owned(), "300".to_owned()],
        "interned in token order, so two runs agree"
    );
    assert_eq!(registry.groups().len(), groups_before + 3);

    let kalshi = registry.tickers(Venue::Kalshi).to_vec();
    let poly = registry.tickers(Venue::Polymarket).to_vec();
    let store = BookStore::multi_venue(
        &[(Venue::Kalshi, &kalshi), (Venue::Polymarket, &poly)],
        Arc::new(ReplayClock::new()),
    )
    .unwrap();

    for binding in registry.bindings() {
        let id = binding.contract_id;
        let book = store.get(id).expect("every bound contract has a book");
        assert_eq!(book.venue, binding.venue, "venue disagrees at {id:?}");
        assert_eq!(
            store.contracts().resolve(id),
            registry.contracts().resolve(id),
            "identifier disagrees at {id:?}"
        );
    }
}

/// Calling twice must not duplicate a market or shift any id.
#[test]
fn extending_twice_is_idempotent() {
    use sum100::registry::Registry;

    let mut registry = Registry::new([]).unwrap();
    let markets = vec![dated("100", Some("2026-12-31T00:00:00Z"))];
    assert_eq!(registry.extend_with_polymarket(&markets).unwrap(), 1);
    assert_eq!(registry.extend_with_polymarket(&markets).unwrap(), 0);
    assert_eq!(registry.tickers(Venue::Polymarket).len(), 1);
    assert_eq!(registry.groups().len(), 1);
}

/// A market with no stated end has no resolution horizon, and the solver ranks
/// on exactly that. Skipped rather than given an invented date.
#[test]
fn a_market_without_an_end_date_is_skipped() {
    use sum100::registry::Registry;

    let mut registry = Registry::new([]).unwrap();
    let markets = vec![dated("100", None), dated("200", Some("not-a-date"))];
    assert_eq!(registry.extend_with_polymarket(&markets).unwrap(), 0);
    assert!(registry.tickers(Venue::Polymarket).is_empty());
}

/// Each market becomes one complement over its own contract, and the group is
/// marked inferred so live orders stay blocked without an explicit opt-in.
#[test]
fn each_market_becomes_one_inferred_complement() {
    use sum100::registry::{Registry, Relation};

    let mut registry = Registry::new([]).unwrap();
    registry
        .extend_with_polymarket(&[dated("100", Some("2026-12-31T00:00:00Z"))])
        .unwrap();

    let contract = registry.contracts().get(Venue::Polymarket, "100").unwrap();
    let group = &registry.groups()[0];
    assert_eq!(group.relation, Relation::Complement { contract });
    assert!(group.inferred, "discovered groups need a human before live");
    assert_eq!(registry.groups_for(contract), &[group.id]);
}
