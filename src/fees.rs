use crate::types::{Cents, ContractId, Venue};
use std::collections::HashMap;

pub trait FeeModel {
    /// Taker fee for `qty` contracts filled at `price`.
    ///
    /// The contract is passed because a venue may price the same trade
    /// differently per market: Polymarket sets a rate per market, so a model
    /// that saw only price and size could not charge the right one. Kalshi's
    /// schedule is uniform and ignores it.
    fn taker_fee(&self, contract: ContractId, price: Cents, qty: i64) -> Cents;
}

#[derive(Clone)]
pub struct KalshiFees {
    pub multiplier_numer: i64,
    pub multiplier_denom: i64,
}

impl Default for KalshiFees {
    fn default() -> Self {
        KalshiFees {
            multiplier_numer: 1,
            multiplier_denom: 1,
        }
    }
}

impl FeeModel for KalshiFees {
    /// Uniform across markets, so the contract is not consulted.
    fn taker_fee(&self, _contract: ContractId, price: Cents, qty: i64) -> Cents {
        let numer = 7 * qty * price * (100 - price) * self.multiplier_numer;
        let denom = 10_000 * self.multiplier_denom;
        (numer + denom - 1) / denom
    }
}

/// What the venue charges a taker, by market category.
///
/// Rates are the venue's published figures, held in basis points so the fee
/// stays integer arithmetic. Makers are never charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeCategory {
    Crypto,
    Sports,
    PoliticsFinance,
    Zero,
}

impl FeeCategory {
    /// Map Gamma's `feeType` onto a category.
    ///
    /// An unrecognized value is treated as the most expensive category rather
    /// than as free. A new `feeType` is far more likely to be a category this
    /// list has not caught up with than a market that stopped charging, and
    /// guessing "free" is the one error that manufactures edge.
    pub fn from_fee_type(fee_type: &str) -> Self {
        match fee_type {
            "zero_fees" => FeeCategory::Zero,
            t if t.starts_with("crypto") => FeeCategory::Crypto,
            t if t.starts_with("sports") => FeeCategory::Sports,
            t if t.starts_with("politics") || t.starts_with("finance") => {
                FeeCategory::PoliticsFinance
            }
            _ => FeeCategory::Crypto,
        }
    }

    pub fn rate_bps(self) -> i64 {
        match self {
            FeeCategory::Crypto => 700,
            FeeCategory::Sports => 500,
            FeeCategory::PoliticsFinance => 400,
            FeeCategory::Zero => 0,
        }
    }
}

/// Polymarket's taker schedule: `size * rate * p * (1 - p)`, per market.
///
/// The rate is not uniform across the venue. Gamma reports it per market as
/// `feeType`, so this holds one category per contract and falls back to the
/// most expensive when a contract has not been registered.
///
/// That fallback is the whole safety argument. Charging too much costs a missed
/// opportunity; charging too little invents edge that is not there and puts real
/// money behind it. An unregistered contract is an unknown, and the safe reading
/// of an unknown fee is the highest one the venue charges — which is also why
/// this is no longer a zero default.
#[derive(Clone, Default)]
pub struct PolymarketFees {
    by_contract: HashMap<ContractId, FeeCategory>,
}

impl PolymarketFees {
    /// Record the category a market charges, as discovery learns it.
    pub fn register(&mut self, contract: ContractId, fee_type: &str) {
        self.by_contract
            .insert(contract, FeeCategory::from_fee_type(fee_type));
    }

    /// The category charged on this contract, or the conservative default.
    pub fn category(&self, contract: ContractId) -> FeeCategory {
        self.by_contract
            .get(&contract)
            .copied()
            .unwrap_or(FeeCategory::Crypto)
    }

    pub fn is_registered(&self, contract: ContractId) -> bool {
        self.by_contract.contains_key(&contract)
    }
}

/// The fee schedule for every venue the engine can trade, selected by [`Venue`].
///
/// A multi-venue position is only correctly costed if each leg is charged by its
/// own venue, so the solver never takes a single fee model; it takes this and
/// looks the leg's venue up.
#[derive(Clone, Default)]
pub struct FeeModels {
    pub kalshi: KalshiFees,
    pub polymarket: PolymarketFees,
}

impl FeeModels {
    pub fn for_venue(&self, venue: Venue) -> &dyn FeeModel {
        match venue {
            Venue::Kalshi => &self.kalshi,
            Venue::Polymarket => &self.polymarket,
        }
    }
}

impl FeeModel for PolymarketFees {
    /// `ceil(qty * rate * p * (1 - p))` in cents, with `p` the price in dollars.
    ///
    /// Written over integers, like the Kalshi schedule beside it: a float here
    /// would let two ways of reaching the same fill round differently, and the
    /// property that splitting an order never saves money is asserted against
    /// exact arithmetic. `i128` because `qty * bps * price * (100 - price)` can
    /// reach 10^13 before the divide.
    fn taker_fee(&self, contract: ContractId, price: Cents, qty: i64) -> Cents {
        let rate_bps = self.category(contract).rate_bps();
        // Dollars: qty * (bps / 10_000) * (price / 100) * ((100 - price) / 100).
        // Cents is a hundred times that, so the divisor loses one factor of 100.
        let numer =
            i128::from(qty) * i128::from(rate_bps) * i128::from(price) * i128::from(100 - price);
        const DENOM: i128 = 10_000 * 100;
        let cents = numer.div_euclid(DENOM) + i128::from(numer.rem_euclid(DENOM) != 0);
        i64::try_from(cents).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kalshi's published taker schedule is ceil(0.07 * C * P * (1 - P)) with P
    /// in dollars. These four points pin the integer arithmetic to the schedule
    /// exactly, including the round-up, since an off-by-one cent in the fee is
    /// enough to flip a marginal set from profitable to loss-making.
    #[test]
    fn kalshi_taker_fee_matches_published_schedule() {
        let fees = KalshiFees::default();
        assert_eq!(fees.taker_fee(ContractId(0), 4, 100), 27);
        assert_eq!(fees.taker_fee(ContractId(0), 62, 100), 165);
        assert_eq!(fees.taker_fee(ContractId(0), 29, 100), 145);
        assert_eq!(fees.taker_fee(ContractId(0), 3, 100), 21);
    }

    /// The venue's published formula is `C * rate * p * (1 - p)` in USDC, so a
    /// hundred Crypto contracts at fifty cents cost `100 * 0.07 * 0.25` of a
    /// dollar: 1.75, or 175 cents. Pinned because the obvious mis-derivation —
    /// scaling by the notional as well as the price — halves it, and a fee that
    /// reads half its true size manufactures edge that is not there.
    #[test]
    fn polymarket_taker_fee_matches_the_published_formula() {
        let mut fees = PolymarketFees::default();
        fees.register(ContractId(0), "crypto_fees_v2");
        fees.register(ContractId(1), "sports_fees_v3");
        fees.register(ContractId(2), "politics_fees");
        fees.register(ContractId(3), "zero_fees");

        assert_eq!(fees.taker_fee(ContractId(0), 50, 100), 175);
        assert_eq!(fees.taker_fee(ContractId(1), 50, 100), 125);
        assert_eq!(fees.taker_fee(ContractId(2), 50, 100), 100);
        assert_eq!(fees.taker_fee(ContractId(3), 50, 100), 0);

        // The curve peaks at the midpoint and vanishes at both ends, which is
        // what makes a near-certain outcome cheap to take.
        assert_eq!(fees.taker_fee(ContractId(0), 10, 100), 63);
        assert_eq!(fees.taker_fee(ContractId(0), 90, 100), 63);
        assert_eq!(fees.taker_fee(ContractId(0), 0, 100), 0);
        assert_eq!(fees.taker_fee(ContractId(0), 100, 100), 0);
    }

    /// A market nobody registered is charged the most expensive category, not
    /// nothing. Charging too much costs an opportunity; charging too little
    /// puts money behind edge that does not exist.
    #[test]
    fn an_unregistered_market_is_charged_the_dearest_rate() {
        let fees = PolymarketFees::default();
        assert!(!fees.is_registered(ContractId(7)));
        assert_eq!(fees.category(ContractId(7)), FeeCategory::Crypto);
        assert_eq!(fees.taker_fee(ContractId(7), 50, 100), 175);

        // And an unfamiliar feeType is read the same way: a category this list
        // has not caught up with, rather than a market that stopped charging.
        let mut later = PolymarketFees::default();
        later.register(ContractId(7), "some_new_fees_v9");
        assert_eq!(later.category(ContractId(7)), FeeCategory::Crypto);
    }

    /// Registration is per market, so two contracts on one venue can be charged
    /// differently in the same trade.
    #[test]
    fn rates_are_held_per_market_not_per_venue() {
        let mut fees = PolymarketFees::default();
        fees.register(ContractId(0), "zero_fees");
        fees.register(ContractId(1), "crypto_fees_v2");
        assert_eq!(fees.taker_fee(ContractId(0), 50, 100), 0);
        assert_eq!(fees.taker_fee(ContractId(1), 50, 100), 175);
    }

    /// The same property the Kalshi schedule holds: splitting a fill can never
    /// cost less than taking it at once, or the solver could be talked into a
    /// cheaper fee than it will actually be charged.
    #[test]
    fn polymarket_fees_never_fall_and_splitting_never_saves() {
        let mut fees = PolymarketFees::default();
        fees.register(ContractId(0), "crypto_fees_v2");
        for price in 0..=100 {
            let mut previous = fees.taker_fee(ContractId(0), price, 0);
            for qty in 0..40 {
                let whole = fees.taker_fee(ContractId(0), price, qty);
                assert!(whole >= previous, "fee fell at p={price} q={qty}");
                previous = whole;
                for split in 0..=qty {
                    let parts = fees.taker_fee(ContractId(0), price, split)
                        + fees.taker_fee(ContractId(0), price, qty - split);
                    assert!(parts >= whole, "splitting saved at p={price} q={qty}");
                }
            }
        }
    }
}

#[cfg(test)]
mod ladder_tests {
    use super::*;

    /// Verify the defining inequalities of the ceiling, independently of the
    /// implementation's add-denominator-minus-one division. This catches both
    /// understating the fee and charging an unnecessary extra cent.
    #[test]
    fn every_tick_is_the_smallest_cent_covering_the_exact_fee() {
        for price in 1..=99 {
            for qty in [0, 1, 2, 7, 99, 100, 101, 499, 500] {
                for (numer, denom) in [(1, 1), (1, 2), (3, 2), (2, 1)] {
                    let model = KalshiFees {
                        multiplier_numer: numer,
                        multiplier_denom: denom,
                    };
                    let fee = model.taker_fee(ContractId(0), price, qty);
                    let exact_numerator = 7i128
                        * i128::from(qty)
                        * i128::from(price)
                        * i128::from(100 - price)
                        * i128::from(numer);
                    let denominator = 10_000i128 * i128::from(denom);
                    let charged = i128::from(fee) * denominator;
                    assert!(
                        charged >= exact_numerator,
                        "undercharge at p={price}, q={qty}"
                    );
                    assert!(
                        charged - exact_numerator < denominator,
                        "excess cent at p={price}, q={qty}"
                    );
                    assert_eq!(fee, model.taker_fee(ContractId(0), 100 - price, qty));
                }
            }
        }
        assert_eq!(KalshiFees::default().taker_fee(ContractId(0), 0, 100), 0);
        assert_eq!(KalshiFees::default().taker_fee(ContractId(0), 100, 100), 0);
    }
}
