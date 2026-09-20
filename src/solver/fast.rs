//! The four closed-form constraint checks.
//!
//! Each one answers a single question in a handful of integer operations: are
//! the best prices in this group mutually inconsistent? They run on every dirty
//! group, so they do no allocation unless they find something and never touch
//! depth, fees, or the clock. Everything expensive happens in
//! [`crate::solver::costing`], and only for groups that fail one of these tests.
//!
//! All four are expressed the same way: a trade is a candidate when the legs
//! that guarantee a dollar can be bought for less than a dollar. Writing the
//! checks in terms of *executable* prices rather than quoted ones matters. A
//! ladder inversion between two mid prices is not a trade if crossing both
//! spreads costs more than the inversion is worth, so the checks compare the
//! price you can actually pay against the price you can actually receive.

use crate::registry::GroupId;
use crate::solver::types::{Candidate, CandidateLeg};
use crate::types::{Book, Cents, Side};

/// Cheapest executable price for the yes outcome, from resting no bids.
fn best_ask_yes(book: &Book) -> Option<Cents> {
    book.best_ask().map(|level| level.price)
}

/// Cheapest executable price for the no outcome, from resting yes bids.
fn best_ask_no(book: &Book) -> Option<Cents> {
    book.best_no_ask().map(|level| level.price)
}

fn leg(book: &Book, side: Side) -> CandidateLeg {
    CandidateLeg {
        venue: book.venue,
        contract_id: book.contract_id,
        side,
    }
}

fn push_if_underpriced(
    group: GroupId,
    legs: Vec<CandidateLeg>,
    total_cost_cents: Cents,
    out: &mut Vec<Candidate>,
) {
    // Exactly one dollar is guaranteed, so anything strictly cheaper is a
    // candidate. Equality is not: a set costing exactly 100 has zero gross edge
    // and no fee schedule can improve it.
    if total_cost_cents < 100 {
        out.push(Candidate {
            group,
            legs,
            top_of_book_cost_cents: total_cost_cents,
        });
    }
}

/// A contract and its own negation must sum to exactly 100 cents.
///
/// The published form of this check is stated two ways — best yes bid plus best
/// no bid above 100, or best yes ask plus best no ask below 100 — and on a
/// single Kalshi contract they are the same inequality, because a yes ask is a
/// resting no bid at `100 - P`. Substituting gives
/// `ask_yes + ask_no = 200 - (bid_yes + bid_no)`, so one is below 100 exactly
/// when the other is above it. Only one needs checking, and the ask form is the
/// one used here because it is the form that gets costed.
///
/// The trade buys both outcomes: one of them pays a dollar, and the other pays
/// nothing, with certainty. The two legs take liquidity from opposite sides of
/// the same book, so they never consume each other's depth.
pub fn evaluate_complement(group: GroupId, book: &Book, out: &mut Vec<Candidate>) {
    let (Some(ask_yes), Some(ask_no)) = (best_ask_yes(book), best_ask_no(book)) else {
        return;
    };
    push_if_underpriced(
        group,
        vec![leg(book, Side::Yes), leg(book, Side::No)],
        ask_yes + ask_no,
        out,
    );
}

/// N mutually exclusive and exhaustive outcomes must sum to 100 cents.
///
/// Exactly one member resolves yes, so buying one of each pays exactly a dollar
/// whatever happens. The check is a sum and a comparison. Fees are deliberately
/// not applied here: they depend on the quantity, which depends on depth, which
/// this stage does not look at. Under-100 is the necessary condition, and
/// costing decides whether it survives.
pub fn evaluate_exhaustive(group: GroupId, books: &[&Book], out: &mut Vec<Candidate>) {
    if books.len() < 2 {
        return;
    }
    let mut total_cost_cents = 0;
    let mut legs = Vec::with_capacity(books.len());
    for book in books {
        let Some(ask_yes) = best_ask_yes(book) else {
            return;
        };
        total_cost_cents += ask_yes;
        legs.push(leg(book, Side::Yes));
    }
    push_if_underpriced(group, legs, total_cost_cents, out);
}

/// A threshold ladder's prices must be non-increasing as the threshold rises.
///
/// `ordered` runs from the weakest claim to the strongest, so each rung's yes
/// event contains the next rung's and `P(rung i) >= P(rung i+1)`. An inversion
/// is tradeable when the weaker rung can be *bought* for less than the stronger
/// rung can be *sold*: buy yes on rung `i`, buy no on rung `i + 1`. That
/// position pays a dollar in every state and two dollars in the one state
/// between the thresholds, so a dollar is the guarantee and the rest is upside.
///
/// Note the direction. The published wording says to buy the higher threshold,
/// but the cheap leg in an inversion is the *lower* threshold — the rung that
/// must be at least as likely and is quoted as though it were less. Buying the
/// dearer, stronger claim and selling the cheaper, weaker one is the losing side
/// of the same trade.
///
/// Every violating adjacent pair becomes its own candidate. A ladder can be
/// inverted in more than one place, and the pairs are priced independently
/// because they consume different books.
pub fn evaluate_monotonicity(group: GroupId, ordered: &[&Book], out: &mut Vec<Candidate>) {
    for pair in ordered.windows(2) {
        let (weak, strong) = (pair[0], pair[1]);
        let (Some(ask_yes), Some(ask_no)) = (best_ask_yes(weak), best_ask_no(strong)) else {
            continue;
        };
        push_if_underpriced(
            group,
            vec![leg(weak, Side::Yes), leg(strong, Side::No)],
            ask_yes + ask_no,
            out,
        );
    }
}

/// The same event listed on two venues must price the same.
///
/// Buying yes on one venue and no on the other pays exactly a dollar, since the
/// two contracts resolve together by definition of the pair. Both directions are
/// checked because either venue can be the cheap one, and they take liquidity
/// from opposite sides of both books, so a group that is crossed in both
/// directions yields two independent trades rather than one double-counted one.
///
/// Callers must confirm the pair is verified before calling. An unverified pair
/// is two contracts that merely look alike, and a cross-venue position on two
/// contracts that settle differently is a naked directional bet.
pub fn evaluate_cross_venue(group: GroupId, a: &Book, b: &Book, out: &mut Vec<Candidate>) {
    for (long, short) in [(a, b), (b, a)] {
        let (Some(ask_yes), Some(ask_no)) = (best_ask_yes(long), best_ask_no(short)) else {
            continue;
        };
        push_if_underpriced(
            group,
            vec![leg(long, Side::Yes), leg(short, Side::No)],
            ask_yes + ask_no,
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContractId, Level, Venue};

    /// Book quoting `ask_yes` for the yes outcome and `ask_no` for the no one.
    ///
    /// A yes ask at A is a resting no bid at `100 - A`; a no ask at B is a
    /// resting yes bid at `100 - B`.
    fn quote(id: u32, ask_yes: Cents, ask_no: Cents, size: i64) -> Book {
        let mut book = Book::new(Venue::Kalshi, ContractId(id));
        book.apply_snapshot(
            &[Level {
                price: 100 - ask_no,
                size,
            }],
            &[Level {
                price: 100 - ask_yes,
                size,
            }],
            1,
            1_000,
        )
        .unwrap();
        assert_eq!(book.best_ask().map(|l| l.price), Some(ask_yes));
        assert_eq!(book.best_no_ask().map(|l| l.price), Some(ask_no));
        book
    }

    #[test]
    fn complement_fires_only_when_both_outcomes_cost_under_a_dollar() {
        let mut out = Vec::new();
        // Crossed: yes bid 60 + no bid 45 = 105, so asks are 55 and 40.
        evaluate_complement(GroupId(0), &quote(0, 55, 40, 10), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].top_of_book_cost_cents, 95);
        assert_eq!(out[0].legs[0].side, Side::Yes);
        assert_eq!(out[0].legs[1].side, Side::No);

        // The bid form of the same test must agree with the ask form.
        let crossed = quote(0, 55, 40, 10);
        assert!(crossed.is_crossed());

        out.clear();
        evaluate_complement(GroupId(0), &quote(0, 55, 46, 10), &mut out);
        assert!(out.is_empty(), "101 cents is not an arbitrage");
        evaluate_complement(GroupId(0), &quote(0, 55, 45, 10), &mut out);
        assert!(out.is_empty(), "exactly 100 cents has no gross edge");
    }

    #[test]
    fn exhaustive_sums_asks_and_needs_every_member_quoted() {
        let books = [
            quote(0, 3, 97, 10),
            quote(1, 60, 40, 10),
            quote(2, 29, 71, 10),
        ];
        let refs: Vec<&Book> = books.iter().collect();
        let mut out = Vec::new();
        evaluate_exhaustive(GroupId(0), &refs, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].top_of_book_cost_cents, 92);
        assert_eq!(out[0].legs.len(), 3);

        // One member with no resting liquidity means no computable sum, so no
        // candidate — never a candidate priced off the members that do quote.
        let empty = Book::new(Venue::Kalshi, ContractId(3));
        let refs = vec![&books[0], &books[1], &empty];
        out.clear();
        evaluate_exhaustive(GroupId(0), &refs, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn monotonicity_buys_the_weaker_rung_and_sells_the_stronger() {
        // Rung 0 is the weaker claim. Buying it at 50 and selling rung 1 at its
        // bid of 62 costs 50 + 38 = 88 for a guaranteed dollar.
        let rungs = [
            quote(0, 60, 40, 10),
            quote(1, 50, 50, 10),
            quote(2, 62, 38, 10),
        ];
        let refs: Vec<&Book> = rungs.iter().collect();
        let mut out = Vec::new();
        evaluate_monotonicity(GroupId(0), &refs, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].top_of_book_cost_cents, 88);
        assert_eq!(out[0].legs[0].contract_id, ContractId(1));
        assert_eq!(out[0].legs[0].side, Side::Yes);
        assert_eq!(out[0].legs[1].contract_id, ContractId(2));
        assert_eq!(out[0].legs[1].side, Side::No);

        // A properly ordered ladder produces nothing.
        let ordered = [
            quote(0, 60, 40, 10),
            quote(1, 50, 50, 10),
            quote(2, 40, 60, 10),
        ];
        let refs: Vec<&Book> = ordered.iter().collect();
        out.clear();
        evaluate_monotonicity(GroupId(0), &refs, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn cross_venue_checks_both_directions_independently() {
        let mut kalshi = quote(0, 45, 55, 10);
        kalshi.venue = Venue::Kalshi;
        let mut polymarket = Book::new(Venue::Polymarket, ContractId(1));
        // Polymarket quotes yes at 52, no at 50: buying Kalshi yes at 45 and
        // Polymarket no at 50 costs 95.
        polymarket
            .apply_snapshot(
                &[Level {
                    price: 50,
                    size: 10,
                }],
                &[Level {
                    price: 48,
                    size: 10,
                }],
                1,
                1_000,
            )
            .unwrap();
        let mut out = Vec::new();
        evaluate_cross_venue(GroupId(0), &kalshi, &polymarket, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].top_of_book_cost_cents, 95);
        assert_eq!(out[0].legs[0].venue, Venue::Kalshi);
        assert_eq!(out[0].legs[1].venue, Venue::Polymarket);
    }
}
