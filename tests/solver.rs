//! Phase 4 solver tests: the four fast paths, the costing pipeline, and the
//! property tests that pin the central no-arbitrage invariant.
//!
//! Money is integer cents throughout. Where a test writes a fee or an edge as a
//! literal, that literal was derived from the published Kalshi schedule by hand
//! and is the point of the test: a plausible-looking number computed the wrong
//! way is exactly the failure mode this suite exists to catch.

use std::{fs, path::Path, sync::Arc};
use sum100::{
    book::BookStore,
    clock::ReplayClock,
    fees::{FeeModel, FeeModels, KalshiFees},
    registry::{GroupId, Registry, Relation},
    solver::{BookSource, Leg, Opportunity, RejectReason, Solver, SolverConfig},
    types::{Book, BookState, Cents, ContractId, Level, Side, Venue},
};

const NOW: u64 = 1_789_343_120_404;
const DAY_MS: u64 = 86_400_000;
/// Far enough out that the annualized threshold is a real gate, not a formality.
const IN_30_DAYS: u64 = NOW + 30 * DAY_MS;

// ---------------------------------------------------------------- test fixtures

/// Books addressed by contract id, with the engine clock frozen.
struct TestBooks {
    books: Vec<Book>,
    now_ms: u64,
}

impl BookSource for TestBooks {
    fn book(&self, contract: ContractId) -> Option<&Book> {
        self.books.iter().find(|b| b.contract_id == contract)
    }

    fn now_ms(&self) -> u64 {
        self.now_ms
    }
}

/// Build a book from the ladders it should *quote*, not the ones it stores.
///
/// A yes ask at A is a resting no bid at `100 - A`; a no ask at B is a resting
/// yes bid at `100 - B`. Writing fixtures in executable prices keeps every test
/// stated in the terms the trade is actually priced in.
fn quoting(
    venue: Venue,
    id: u32,
    yes_asks: &[(Cents, i64)],
    no_asks: &[(Cents, i64)],
    ts_ms: u64,
) -> Book {
    let flip = |ladder: &[(Cents, i64)]| -> Vec<Level> {
        ladder
            .iter()
            .map(|(price, size)| Level {
                price: 100 - price,
                size: *size,
            })
            .collect()
    };
    let mut book = Book::new(venue, ContractId(id));
    book.apply_snapshot(&flip(no_asks), &flip(yes_asks), 1, ts_ms)
        .unwrap();
    book
}

/// A member of an exhaustive set: one yes ask, one price, one depth.
fn member(id: u32, ask_yes: Cents, size: i64) -> Book {
    quoting(Venue::Kalshi, id, &[(ask_yes, size)], &[], NOW)
}

/// A ladder rung quoting `ask` with a one cent spread beneath it.
fn rung(id: u32, ask_yes: Cents, size: i64) -> Book {
    quoting(
        Venue::Kalshi,
        id,
        &[(ask_yes, size)],
        &[(100 - (ask_yes - 1), size)],
        NOW,
    )
}

fn exhaustive(members: &[u32]) -> Registry {
    Registry::new([(
        Relation::Exhaustive {
            members: members.iter().map(|id| ContractId(*id)).collect(),
        },
        IN_30_DAYS,
    )])
    .unwrap()
}

fn evaluate(registry: &Registry, books: &TestBooks, config: &SolverConfig) -> Vec<Opportunity> {
    let dirty: Vec<ContractId> = books.books.iter().map(|b| b.contract_id).collect();
    Solver::new().evaluate(registry, books, &dirty, &FeeModels::default(), config)
}

/// Run the solver and return the single rejection reason it recorded.
fn sole_rejection(registry: &Registry, books: &TestBooks, config: &SolverConfig) -> RejectReason {
    let dirty: Vec<ContractId> = books.books.iter().map(|b| b.contract_id).collect();
    let mut solver = Solver::new();
    let found = solver.evaluate(registry, books, &dirty, &FeeModels::default(), config);
    assert!(found.is_empty(), "expected no opportunity, got {found:?}");
    let m = &solver.metrics;
    let reasons = [
        (RejectReason::NotLive, m.rejected_not_live),
        (RejectReason::Stale, m.rejected_stale),
        (RejectReason::MissingBook, m.rejected_missing_book),
        (RejectReason::Unverified, m.rejected_unverified),
        (RejectReason::NoDepth, m.rejected_no_depth),
        (RejectReason::FeesExceedGap, m.rejected_fees_exceed_gap),
        (RejectReason::BelowMinEdge, m.rejected_below_min_edge),
        (RejectReason::BelowMinReturn, m.rejected_below_min_return),
        (
            RejectReason::PayoffNotGuaranteed,
            m.rejected_payoff_not_guaranteed,
        ),
    ];
    let hits: Vec<RejectReason> = reasons
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(reason, _)| *reason)
        .collect();
    assert_eq!(hits.len(), 1, "expected exactly one reason, got {hits:?}");
    assert_eq!(m.rejections(), 1);
    hits[0]
}

/// Payoff of a position in one resolution, in cents.
fn payoff_in_state(legs: &[Leg], state: &[ContractId]) -> Cents {
    legs.iter()
        .map(|leg| {
            let resolves_yes = state.contains(&leg.contract_id);
            let pays = match leg.side {
                Side::Yes => resolves_yes,
                Side::No => !resolves_yes,
            };
            if pays { leg.qty * 100 } else { 0 }
        })
        .sum()
}

// ------------------------------------------------------- the Fed example, costed

/// The 98 cent set from the project's founding example. Fees eat the gap.
#[test]
fn fed_example_at_98_cents_is_rejected_for_fees() {
    let prices: [Cents; 4] = [4, 62, 29, 3];
    assert_eq!(prices.iter().sum::<Cents>(), 98);
    let books = TestBooks {
        books: prices
            .iter()
            .enumerate()
            .map(|(i, p)| member(i as u32, *p, 100))
            .collect(),
        now_ms: NOW,
    };
    let registry = exhaustive(&[0, 1, 2, 3]);
    assert_eq!(
        sole_rejection(&registry, &books, &SolverConfig::default()),
        RejectReason::FeesExceedGap
    );
}

/// The 95 cent version clears the same schedule, with the edge pinned exactly.
#[test]
fn fed_example_at_95_cents_is_accepted_with_the_expected_net_edge() {
    let prices: [Cents; 4] = [3, 60, 29, 3];
    assert_eq!(prices.iter().sum::<Cents>(), 95);
    let books = TestBooks {
        books: prices
            .iter()
            .enumerate()
            .map(|(i, p)| member(i as u32, *p, 100))
            .collect(),
        now_ms: NOW,
    };
    let found = evaluate(&exhaustive(&[0, 1, 2, 3]), &books, &SolverConfig::default());
    assert_eq!(found.len(), 1);
    let opportunity = &found[0];

    let fees = KalshiFees::default();
    let expected_fees: Cents = prices.iter().map(|p| fees.taker_fee(*p, 100)).sum();
    assert_eq!(expected_fees, 21 + 168 + 145 + 21);
    assert_eq!(opportunity.qty, 100);
    assert_eq!(opportunity.guaranteed_payoff_cents, 10_000);
    assert_eq!(opportunity.gross_cents, 500);
    assert_eq!(opportunity.fees_cents, 355);
    assert_eq!(opportunity.net_cents, 145);
    assert_eq!(opportunity.capital_cents, 9_855);
    assert_eq!(opportunity.legs.len(), 4);
    // 1.47% on locked capital over 30 days is 17.9% annualized, which clears
    // the 15% floor. The same trade eight months out would not.
    assert!((opportunity.annualized_return_percent() - 17.90).abs() < 0.01);
}

/// The table the phase 4 deliverable asks for, printed from the real solver.
///
/// Run with `cargo test --test solver -- --nocapture fed_example_rejection_table`.
#[test]
fn fed_example_rejection_table() {
    // Scoped rather than global: tests share a process, and a global subscriber
    // would scatter this test's log lines through everyone else's output.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, fed_example_table_body);
}

fn fed_example_table_body() {
    let fees = KalshiFees::default();
    println!("\n{:-<72}", "");
    println!("Fed example, exhaustive set of four outcomes, 100 contracts per leg");
    println!("{:-<72}", "");
    println!(
        "{:<14} {:>10} {:>12} {:>12} {:>12}",
        "case", "leg price", "leg cost", "leg fee", "running"
    );
    for (label, prices) in [
        ("98c (reject)", [4, 62, 29, 3]),
        ("95c (accept)", [3, 60, 29, 3]),
    ] {
        let mut cost = 0;
        let mut fee = 0;
        for price in prices {
            cost += price * 100;
            fee += fees.taker_fee(price, 100);
            println!(
                "{label:<14} {price:>10} {:>12} {:>12} {:>12}",
                price * 100,
                fees.taker_fee(price, 100),
                cost + fee
            );
        }
        let books = TestBooks {
            books: prices
                .iter()
                .enumerate()
                .map(|(i, p)| member(i as u32, *p, 100))
                .collect(),
            now_ms: NOW,
        };
        let mut solver = Solver::new();
        let dirty: Vec<ContractId> = (0..4).map(ContractId).collect();
        let found = solver.evaluate(
            &exhaustive(&[0, 1, 2, 3]),
            &books,
            &dirty,
            &FeeModels::default(),
            &SolverConfig::default(),
        );
        println!(
            "{label:<14} payoff 10000  cost {cost}  fees {fee}  net {}  -> {}",
            10_000 - cost - fee,
            if found.is_empty() {
                "rejected".to_string()
            } else {
                format!(
                    "accepted, {:.2}% annualized",
                    found[0].annualized_return_percent()
                )
            }
        );
        println!("{:-<72}", "");
    }
}

// --------------------------------------------------------- the four fast paths

#[test]
fn complement_path_prices_both_outcomes_of_one_contract() {
    // Crossed book: yes bid 60 and no bid 45 sum to 105, so buying both
    // outcomes costs 55 + 40 = 95 for a guaranteed dollar.
    let book = quoting(Venue::Kalshi, 0, &[(55, 100)], &[(40, 100)], NOW);
    assert!(book.is_crossed());
    let books = TestBooks {
        books: vec![book],
        now_ms: NOW,
    };
    let registry = Registry::new([(
        Relation::Complement {
            contract: ContractId(0),
        },
        IN_30_DAYS,
    )])
    .unwrap();

    let found = evaluate(&registry, &books, &SolverConfig::default());
    assert_eq!(found.len(), 1);
    let opportunity = &found[0];
    assert_eq!(opportunity.qty, 100);
    assert_eq!(opportunity.gross_cents, 500);
    // Kalshi's schedule is symmetric in price, so the two legs are charged
    // 174 at 55c and 168 at 40c.
    assert_eq!(opportunity.fees_cents, 174 + 168);
    assert_eq!(opportunity.net_cents, 158);
    assert_eq!(opportunity.legs[0].side, Side::Yes);
    assert_eq!(opportunity.legs[1].side, Side::No);
    // The two legs take opposite sides of the same book and never compete for
    // the same resting orders.
    assert_eq!(opportunity.legs[0].total_cost_cents, 5_500);
    assert_eq!(opportunity.legs[1].total_cost_cents, 4_000);
}

/// A 96 cent set whose entire four cent gap is consumed by 3.99 dollars of fees.
///
/// The published fixture asks for "sum 96c, fees 3c, net edge +1c". Under the
/// real Kalshi ceiling schedule no four-leg set can be charged three cents at
/// one contract, because each level rounds up to at least one cent on its own,
/// so a four leg trade costs at least four cents in fees and a four cent gap can
/// never clear at size one. At a hundred contracts the same shape lands where
/// the fixture intends: gross 400, fees 399, net exactly +1.
#[test]
fn ninety_six_cent_set_clears_fees_by_exactly_one_cent() {
    let prices: [Cents; 4] = [4, 4, 46, 42];
    assert_eq!(prices.iter().sum::<Cents>(), 96);
    let books = TestBooks {
        books: prices
            .iter()
            .enumerate()
            .map(|(i, p)| member(i as u32, *p, 100))
            .collect(),
        now_ms: NOW,
    };
    let registry = exhaustive(&[0, 1, 2, 3]);

    // Isolate the fee arithmetic from the return threshold: one cent on 99.99
    // dollars is a real edge and a terrible return.
    let no_return_floor = SolverConfig {
        min_annualized_return: 0.0,
        ..SolverConfig::default()
    };
    let found = evaluate(&registry, &books, &no_return_floor);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].gross_cents, 400);
    assert_eq!(found[0].fees_cents, 27 + 27 + 174 + 171);
    assert_eq!(found[0].net_cents, 1);

    // Under the shipped config the same trade is rejected, on the return and
    // not on the arithmetic: 0.01% over thirty days annualizes to 0.12%.
    assert_eq!(
        sole_rejection(&registry, &books, &SolverConfig::default()),
        RejectReason::BelowMinReturn
    );
}

/// The published ladder, 60/55/58/52, is a genuine inversion that fees kill.
#[test]
fn published_ladder_inversion_is_found_and_then_rejected_for_fees() {
    let books = TestBooks {
        books: vec![
            rung(0, 60, 100),
            rung(1, 55, 100),
            rung(2, 58, 100),
            rung(3, 52, 100),
        ],
        now_ms: NOW,
    };
    let registry = Registry::new([(
        Relation::Monotone {
            ordered: (0..4).map(ContractId).collect(),
        },
        IN_30_DAYS,
    )])
    .unwrap();

    // Rung 1 can be bought at 55 while rung 2 bids 57: a two cent gross edge,
    // against 3.46 dollars of fees at a hundred contracts.
    assert_eq!(
        sole_rejection(&registry, &books, &SolverConfig::default()),
        RejectReason::FeesExceedGap
    );
}

/// A wider inversion in the same shape does clear, with the edge pinned.
#[test]
fn ladder_inversion_wide_enough_to_pay_for_itself_is_accepted() {
    let books = TestBooks {
        books: vec![
            rung(0, 60, 100),
            rung(1, 50, 100),
            rung(2, 62, 100),
            rung(3, 40, 100),
        ],
        now_ms: NOW,
    };
    let registry = Registry::new([(
        Relation::Monotone {
            ordered: (0..4).map(ContractId).collect(),
        },
        IN_30_DAYS,
    )])
    .unwrap();

    let found = evaluate(&registry, &books, &SolverConfig::default());
    assert_eq!(found.len(), 1);
    let opportunity = &found[0];
    // Buy rung 1 yes at 50, sell rung 2 at its bid of 61 (buy its no at 39).
    assert_eq!(opportunity.legs[0].contract_id, ContractId(1));
    assert_eq!(opportunity.legs[0].side, Side::Yes);
    assert_eq!(opportunity.legs[1].contract_id, ContractId(2));
    assert_eq!(opportunity.legs[1].side, Side::No);
    assert_eq!(opportunity.gross_cents, 1_100);
    assert_eq!(opportunity.fees_cents, 175 + 167);
    assert_eq!(opportunity.net_cents, 758);

    // A correctly ordered ladder emits nothing at all.
    let ordered = TestBooks {
        books: vec![
            rung(0, 60, 100),
            rung(1, 50, 100),
            rung(2, 40, 100),
            rung(3, 30, 100),
        ],
        now_ms: NOW,
    };
    assert!(evaluate(&registry, &ordered, &SolverConfig::default()).is_empty());
}

/// An implication is the two-rung case of the ladder, and dispatches there.
#[test]
fn implication_is_evaluated_as_a_two_rung_ladder() {
    // "BTC above 80k" implies "BTC above 70k", so the consequent is the weaker
    // claim and must be at least as likely. Buying the consequent at 50 and
    // selling the antecedent at its bid of 61 costs 89 for a guaranteed dollar.
    let books = TestBooks {
        books: vec![rung(0, 62, 100), rung(1, 50, 100)],
        now_ms: NOW,
    };
    let registry = Registry::new([(
        Relation::Implies {
            antecedent: ContractId(0),
            consequent: ContractId(1),
        },
        IN_30_DAYS,
    )])
    .unwrap();

    let found = evaluate(&registry, &books, &SolverConfig::default());
    assert_eq!(found.len(), 1);
    // The consequent is bought, the antecedent sold — not the other way round.
    assert_eq!(found[0].legs[0].contract_id, ContractId(1));
    assert_eq!(found[0].legs[0].side, Side::Yes);
    assert_eq!(found[0].legs[1].contract_id, ContractId(0));
    assert_eq!(found[0].legs[1].side, Side::No);
    assert_eq!(found[0].gross_cents, 1_100);
    assert_eq!(found[0].net_cents, 758);

    // Stated coherently, the same pair produces nothing.
    let coherent = TestBooks {
        books: vec![rung(0, 40, 100), rung(1, 60, 100)],
        now_ms: NOW,
    };
    assert!(evaluate(&registry, &coherent, &SolverConfig::default()).is_empty());
}

/// Cross-venue equivalence against a mock Polymarket book.
#[test]
fn cross_venue_pair_is_priced_per_venue_and_gated_on_verification() {
    let make = |verified: bool| {
        Registry::new([(
            Relation::Equivalent {
                a: ContractId(0),
                b: ContractId(1),
                verified,
            },
            IN_30_DAYS,
        )])
        .unwrap()
    };
    // Kalshi yes at 45, Polymarket no at 50: 95 cents for a guaranteed dollar.
    let books = TestBooks {
        books: vec![
            quoting(Venue::Kalshi, 0, &[(45, 100)], &[(56, 100)], NOW),
            quoting(Venue::Polymarket, 1, &[(52, 100)], &[(50, 100)], NOW),
        ],
        now_ms: NOW,
    };

    let found = evaluate(&make(true), &books, &SolverConfig::default());
    assert_eq!(found.len(), 1);
    let opportunity = &found[0];
    assert_eq!(opportunity.legs[0].venue, Venue::Kalshi);
    assert_eq!(opportunity.legs[1].venue, Venue::Polymarket);
    assert_eq!(opportunity.gross_cents, 500);
    // Only the Kalshi leg is charged; the Polymarket stub models no taker fee
    // until phase 7 supplies the real schedule.
    assert_eq!(opportunity.legs[0].fee_cents, 174);
    assert_eq!(opportunity.legs[1].fee_cents, 0);
    assert_eq!(opportunity.net_cents, 326);

    // The identical prices produce nothing until a human has verified the pair.
    assert_eq!(
        sole_rejection(&make(false), &books, &SolverConfig::default()),
        RejectReason::Unverified
    );
}

// ----------------------------------------------------------- costing pipeline

/// Depth walking and size capping on a set with uneven, multi-level books.
#[test]
fn depth_walk_costs_every_level_and_the_thinnest_leg_caps_the_trade() {
    let books = TestBooks {
        books: vec![
            member(0, 3, 1_000),
            // Twenty contracts at 60, then the price steps to 61.
            quoting(Venue::Kalshi, 1, &[(60, 20), (61, 500)], &[], NOW),
            member(2, 29, 40),
            member(3, 3, 3_000),
        ],
        now_ms: NOW,
    };
    // A 36 cent edge on 39.64 dollars over thirty days annualizes to 11%, below
    // the shipped floor; this test is about depth and sizing, so the floor is
    // taken out of the way rather than the numbers bent to clear it.
    let config = SolverConfig {
        min_annualized_return: 0.0,
        ..SolverConfig::default()
    };
    let found = evaluate(&exhaustive(&[0, 1, 2, 3]), &books, &config);
    assert_eq!(found.len(), 1);
    let opportunity = &found[0];

    // Sizes are [1000, 520, 40, 3000], so this is a 40 contract trade.
    assert_eq!(opportunity.qty, 40);
    assert!(opportunity.legs.iter().all(|leg| leg.qty == 40));
    // Top of book times quantity would say 2400 on the stepped leg. The truth
    // is 20 at 60 plus 20 at 61.
    assert_eq!(opportunity.legs[1].total_cost_cents, 20 * 60 + 20 * 61);
    assert_eq!(opportunity.fees_cents, 9 + (34 + 34) + 58 + 9);
    assert_eq!(opportunity.net_cents, 36);
    // The stepped leg is charged once per level, not once on a blended price.
    assert_eq!(opportunity.legs[1].fee_cents, 68);
}

#[test]
fn a_stale_or_resyncing_book_skips_the_whole_group() {
    let registry = exhaustive(&[0, 1, 2, 3]);
    let prices: [Cents; 4] = [3, 60, 29, 3];
    let fresh: Vec<Book> = prices
        .iter()
        .enumerate()
        .map(|(i, p)| member(i as u32, *p, 100))
        .collect();

    // One leg 501 ms old against a 500 ms budget.
    let mut stale = fresh.clone();
    stale[2].updated_at_ms = NOW - 501;
    assert_eq!(
        sole_rejection(
            &registry,
            &TestBooks {
                books: stale,
                now_ms: NOW
            },
            &SolverConfig::default()
        ),
        RejectReason::Stale
    );

    // A book awaiting a fresh snapshot is refused whatever its contents say.
    let mut resyncing = fresh.clone();
    resyncing[0].mark_resyncing();
    assert_eq!(
        sole_rejection(
            &registry,
            &TestBooks {
                books: resyncing,
                now_ms: NOW
            },
            &SolverConfig::default()
        ),
        RejectReason::NotLive
    );

    // Exactly at the age budget is still fresh; the gate is inclusive.
    let mut edge = fresh;
    edge[2].updated_at_ms = NOW - 500;
    assert_eq!(
        evaluate(
            &registry,
            &TestBooks {
                books: edge,
                now_ms: NOW
            },
            &SolverConfig::default()
        )
        .len(),
        1
    );
}

#[test]
fn opportunities_are_ranked_by_annualized_return_not_raw_profit() {
    // Two independent sets: a fat edge locked for a year, and a thin one that
    // unlocks in a day.
    let registry = Registry::new([
        (
            Relation::Exhaustive {
                members: vec![ContractId(0), ContractId(1)],
            },
            NOW + 365 * DAY_MS,
        ),
        (
            Relation::Exhaustive {
                members: vec![ContractId(2), ContractId(3)],
            },
            NOW + DAY_MS,
        ),
    ])
    .unwrap();
    let books = TestBooks {
        books: vec![
            member(0, 5, 100),
            member(1, 78, 100),
            member(2, 5, 100),
            member(3, 89, 100),
        ],
        now_ms: NOW,
    };
    let found = evaluate(&registry, &books, &SolverConfig::default());
    assert_eq!(found.len(), 2);
    // The slow set earns more cents and ranks second.
    assert_eq!(found[0].group, GroupId(1));
    assert!(found[0].net_cents < found[1].net_cents);
    assert!(found[0].annualized_return > found[1].annualized_return);
}

/// The real stage 2 capture: a live book that is coherent produces no signal.
#[test]
fn live_fixture_book_is_coherent_and_emits_nothing() {
    let ticker = "KXBTCD-26SEP1417-T76999.99".to_string();
    let tickers = vec![ticker.clone()];
    let clock = ReplayClock::new();
    // Stamp the books at engine time: the store reads this clock as it applies.
    clock.advance_to(NOW);
    let mut parser = sum100::feed::kalshi::Parser::new(&tickers).unwrap();
    let mut store = BookStore::new(Venue::Kalshi, &tickers, Arc::new(clock.clone())).unwrap();
    let raw = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/stage2-live-orderbook.ndjson"),
    )
    .unwrap();
    for line in raw.lines() {
        if let Some(event) = parser.parse(line, NOW) {
            store.apply(&event);
        }
    }

    let book = store.get(ContractId(0)).unwrap();
    assert_eq!(book.state, BookState::Live);
    // Best yes bid 44, best yes ask 46: buying both outcomes costs 46 + 56.
    assert_eq!(book.best_ask().map(|l| l.price), Some(46));
    assert_eq!(book.best_no_ask().map(|l| l.price), Some(56));

    let registry = Registry::new([(
        Relation::Complement {
            contract: ContractId(0),
        },
        IN_30_DAYS,
    )])
    .unwrap();
    let mut solver = Solver::new();
    let found = solver.evaluate(
        &registry,
        &store,
        &[ContractId(0)],
        &FeeModels::default(),
        &SolverConfig::default(),
    );
    assert!(found.is_empty());
    // Coherent, not gated: the group was evaluated and simply had no violation.
    assert_eq!(solver.metrics.groups_evaluated, 1);
    assert_eq!(solver.metrics.candidates_found, 0);
    assert_eq!(solver.metrics.rejections(), 0);
}

// --------------------------------------------------------------- property tests

/// splitmix64. Deterministic by construction, so a failure replays exactly.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next_u64() % ((hi - lo + 1) as u64)) as i64
    }
}

/// Property 1: a group that satisfies its own constraint produces nothing.
#[test]
fn property_coherent_groups_never_produce_a_signal() {
    let mut rng = Rng(0xC0FFEE);
    let config = SolverConfig {
        min_net_edge_cents: 1,
        min_annualized_return: 0.0,
        ..SolverConfig::default()
    };

    for _ in 0..500 {
        // Exhaustive set whose asks sum to at least 100.
        let n = rng.range(2, 5) as usize;
        let mut asks: Vec<Cents> = (0..n).map(|_| rng.range(1, 99)).collect();
        let deficit = 100 - asks.iter().sum::<Cents>();
        if deficit > 0 {
            asks[0] = (asks[0] + deficit).min(99);
            let still_short = 100 - asks.iter().sum::<Cents>();
            if still_short > 0 {
                asks[n - 1] = (asks[n - 1] + still_short).min(99);
            }
        }
        if asks.iter().sum::<Cents>() < 100 {
            continue;
        }
        let books = TestBooks {
            books: asks
                .iter()
                .enumerate()
                .map(|(i, ask)| member(i as u32, *ask, rng.range(1, 500)))
                .collect(),
            now_ms: NOW,
        };
        let registry = exhaustive(&(0..n as u32).collect::<Vec<_>>());
        let found = evaluate(&registry, &books, &config);
        assert!(found.is_empty(), "coherent set {asks:?} produced {found:?}");

        // Uncrossed complement: the two outcomes cost at least a dollar.
        let ask_yes = rng.range(1, 99);
        let ask_no = rng.range(100 - ask_yes, 99).max(100 - ask_yes);
        let books = TestBooks {
            books: vec![quoting(
                Venue::Kalshi,
                0,
                &[(ask_yes, 100)],
                &[(ask_no, 100)],
                NOW,
            )],
            now_ms: NOW,
        };
        let registry = Registry::new([(
            Relation::Complement {
                contract: ContractId(0),
            },
            IN_30_DAYS,
        )])
        .unwrap();
        let found = evaluate(&registry, &books, &config);
        assert!(
            found.is_empty(),
            "uncrossed book {ask_yes}/{ask_no} produced {found:?}"
        );

        // Monotone ladder: each rung's ask is at least the next rung's bid, so
        // no adjacent pair can be crossed.
        let rungs = rng.range(2, 5) as usize;
        let mut asks: Vec<Cents> = Vec::with_capacity(rungs);
        let mut ask = rng.range(rungs as i64 + 1, 99);
        for _ in 0..rungs {
            asks.push(ask);
            ask = (ask - rng.range(1, 5)).max(1);
        }
        let ladder: Vec<Book> = asks
            .iter()
            .enumerate()
            .map(|(i, ask)| {
                // Bid at or below the previous rung's ask keeps the ladder coherent.
                let bid = (*ask - 1).max(1).min(if i == 0 { 99 } else { asks[i - 1] });
                quoting(
                    Venue::Kalshi,
                    i as u32,
                    &[(*ask, 100)],
                    &[(100 - bid, 100)],
                    NOW,
                )
            })
            .collect();
        let registry = Registry::new([(
            Relation::Monotone {
                ordered: (0..rungs as u32).map(ContractId).collect(),
            },
            IN_30_DAYS,
        )])
        .unwrap();
        let found = evaluate(
            &registry,
            &TestBooks {
                books: ladder,
                now_ms: NOW,
            },
            &config,
        );
        assert!(
            found.is_empty(),
            "monotone ladder {asks:?} produced {found:?}"
        );
    }
}

/// Property 2: the central invariant. Every emitted position pays at least its
/// guaranteed payoff in every resolution, and that payoff strictly exceeds
/// everything paid to enter it.
#[test]
fn property_every_opportunity_pays_in_every_resolution() {
    let mut rng = Rng(0x5EED_1234);
    let config = SolverConfig {
        min_net_edge_cents: 1,
        min_annualized_return: 0.0,
        ..SolverConfig::default()
    };
    let mut emitted = 0;

    for _ in 0..2_000 {
        // Random prices, no coherence imposed: most draws are unprofitable and
        // the interesting ones are the few that are not.
        let shape = rng.range(0, 3);
        let (relation, books) = match shape {
            0 => {
                let n = rng.range(2, 5) as usize;
                let books: Vec<Book> = (0..n)
                    .map(|i| member(i as u32, rng.range(1, 60), rng.range(1, 400)))
                    .collect();
                (
                    Relation::Exhaustive {
                        members: (0..n as u32).map(ContractId).collect(),
                    },
                    books,
                )
            }
            1 => {
                let ask_yes = rng.range(1, 99);
                let ask_no = rng.range(1, 99);
                (
                    Relation::Complement {
                        contract: ContractId(0),
                    },
                    vec![quoting(
                        Venue::Kalshi,
                        0,
                        &[(ask_yes, rng.range(1, 400))],
                        &[(ask_no, rng.range(1, 400))],
                        NOW,
                    )],
                )
            }
            2 => {
                let rungs = rng.range(2, 5) as usize;
                let books: Vec<Book> = (0..rungs)
                    .map(|i| {
                        let ask = rng.range(2, 99);
                        quoting(
                            Venue::Kalshi,
                            i as u32,
                            &[(ask, rng.range(1, 400))],
                            &[(rng.range(1, 99), rng.range(1, 400))],
                            NOW,
                        )
                    })
                    .collect();
                (
                    Relation::Monotone {
                        ordered: (0..rungs as u32).map(ContractId).collect(),
                    },
                    books,
                )
            }
            _ => (
                Relation::Equivalent {
                    a: ContractId(0),
                    b: ContractId(1),
                    verified: true,
                },
                vec![
                    quoting(
                        Venue::Kalshi,
                        0,
                        &[(rng.range(1, 99), rng.range(1, 400))],
                        &[(rng.range(1, 99), rng.range(1, 400))],
                        NOW,
                    ),
                    quoting(
                        Venue::Polymarket,
                        1,
                        &[(rng.range(1, 99), rng.range(1, 400))],
                        &[(rng.range(1, 99), rng.range(1, 400))],
                        NOW,
                    ),
                ],
            ),
        };

        let registry = Registry::new([(relation.clone(), IN_30_DAYS)]).unwrap();
        let books = TestBooks { books, now_ms: NOW };
        for opportunity in evaluate(&registry, &books, &config) {
            emitted += 1;
            let outlay = opportunity.capital_cents;
            assert_eq!(
                outlay,
                opportunity
                    .legs
                    .iter()
                    .map(|l| l.total_cost_cents + l.fee_cents)
                    .sum::<Cents>()
            );
            // Non-negative payoff in every possible resolution, and strictly
            // more than the cost of entering: the portfolio is free money.
            for state in relation.resolution_states() {
                let payoff = payoff_in_state(&opportunity.legs, &state);
                assert!(payoff >= 0);
                assert!(
                    payoff >= opportunity.guaranteed_payoff_cents,
                    "state {state:?} pays {payoff}, below the claimed guarantee"
                );
                assert!(
                    payoff - outlay >= opportunity.net_cents,
                    "state {state:?} nets {}, below the claimed {}",
                    payoff - outlay,
                    opportunity.net_cents
                );
            }
            assert!(opportunity.net_cents > 0);
            assert_eq!(
                opportunity.net_cents,
                opportunity.guaranteed_payoff_cents - outlay
            );
            // Every leg holds the same quantity, or the set does not settle flat.
            assert!(opportunity.legs.iter().all(|l| l.qty == opportunity.qty));
        }
    }
    assert!(
        emitted > 50,
        "only {emitted} opportunities generated; the property is close to vacuous"
    );
}

/// Property 3: quantity never buys a discount on fees.
///
/// The published property is that more contracts *strictly* increase total
/// fees. That is false for a ceiling schedule and asserting it would be
/// asserting a bug: at a penny price, a single contract and two contracts both
/// round up to the same one cent. What must hold, and what actually protects
/// the engine, is that fees never fall as quantity rises and that splitting an
/// order into pieces never costs less than placing it whole. Both are checked,
/// along with strict growth once the exact fee has moved by a full cent.
#[test]
fn property_fees_never_fall_and_splitting_never_saves() {
    let mut rng = Rng(0xFEE5);
    for multiplier in [(1, 1), (1, 2), (3, 2), (2, 1)] {
        let fees = KalshiFees {
            multiplier_numer: multiplier.0,
            multiplier_denom: multiplier.1,
        };
        for price in 1..=99 {
            let mut previous = fees.taker_fee(price, 0);
            for qty in 1..=600 {
                let fee = fees.taker_fee(price, qty);
                assert!(
                    fee >= previous,
                    "fee fell from {previous} to {fee} at p={price} q={qty}"
                );
                previous = fee;
            }
            for _ in 0..40 {
                let a = rng.range(1, 400);
                let b = rng.range(1, 400);
                // Subadditive: one order of a+b is never dearer than two orders.
                assert!(
                    fees.taker_fee(price, a + b)
                        <= fees.taker_fee(price, a) + fees.taker_fee(price, b),
                    "splitting beat batching at p={price} a={a} b={b}"
                );
            }
            // Once the exact fee has grown by more than a cent, the charge must
            // have grown too: the ceiling flattens small steps, never large ones.
            let exact = |qty: i64| -> i64 {
                7 * qty * price * (100 - price) * multiplier.0 / (10_000 * multiplier.1)
            };
            for qty in [1i64, 7, 50, 100] {
                let bigger = qty * 4 + 20;
                if exact(bigger) > exact(qty) + 1 {
                    assert!(
                        fees.taker_fee(price, bigger) > fees.taker_fee(price, qty),
                        "fee flat across a full cent at p={price} {qty}->{bigger}"
                    );
                }
            }
        }
    }
}

/// Property 4: the thinnest leg determines the executable quantity.
#[test]
fn property_thinnest_leg_caps_the_executable_quantity() {
    let mut rng = Rng(0x0D3F_7401);
    let config = SolverConfig {
        min_annualized_return: 0.0,
        ..SolverConfig::default()
    };

    for _ in 0..400 {
        // Three cheap members: every level of every leg is profitable, so
        // nothing but depth and the position cap can limit the size.
        let depths: Vec<i64> = (0..3).map(|_| rng.range(1, 900)).collect();
        let books = TestBooks {
            books: depths
                .iter()
                .enumerate()
                .map(|(i, depth)| member(i as u32, 5, *depth))
                .collect(),
            now_ms: NOW,
        };
        let found = evaluate(&exhaustive(&[0, 1, 2]), &books, &config);
        assert_eq!(found.len(), 1, "15 cent set must clear fees at any size");
        let thinnest = *depths.iter().min().unwrap();
        assert_eq!(
            found[0].qty,
            thinnest.min(config.max_position_size),
            "depths {depths:?} should cap at the thinnest leg"
        );
        assert!(found[0].qty <= thinnest);
    }

    // Mixed depth across levels: the cap is total depth per leg, not top level.
    let books = TestBooks {
        books: vec![
            quoting(Venue::Kalshi, 0, &[(5, 3), (6, 4)], &[], NOW),
            member(1, 5, 900),
            member(2, 5, 900),
        ],
        now_ms: NOW,
    };
    let found = evaluate(&exhaustive(&[0, 1, 2]), &books, &config);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].qty, 7);
}

#[test]
fn no_depth_on_any_leg_rejects_the_group() {
    let books = TestBooks {
        books: vec![
            member(0, 3, 100),
            member(1, 60, 100),
            member(2, 29, 100),
            // Quoted, but the only level is exhausted after the snapshot.
            quoting(Venue::Kalshi, 3, &[(3, 1)], &[], NOW),
        ],
        now_ms: NOW,
    };
    // A one contract leg caps the trade at one contract, where four legs of
    // ceiling-rounded fees exceed the five cent gap.
    assert_eq!(
        sole_rejection(&exhaustive(&[0, 1, 2, 3]), &books, &SolverConfig::default()),
        RejectReason::FeesExceedGap
    );

    // A member with no resting liquidity at all never even becomes a candidate.
    let books = TestBooks {
        books: vec![
            member(0, 3, 100),
            member(1, 60, 100),
            member(2, 29, 100),
            Book::new(Venue::Kalshi, ContractId(3)),
        ],
        now_ms: NOW,
    };
    let dirty: Vec<ContractId> = (0..4).map(ContractId).collect();
    let mut solver = Solver::new();
    let found = solver.evaluate(
        &exhaustive(&[0, 1, 2, 3]),
        &books,
        &dirty,
        &FeeModels::default(),
        &SolverConfig::default(),
    );
    assert!(found.is_empty());
    assert_eq!(solver.metrics.rejected_not_live, 1);
}
