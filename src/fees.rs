use crate::types::{Cents, Venue};

pub trait FeeModel {
    fn taker_fee(&self, price: Cents, qty: i64) -> Cents;
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
    fn taker_fee(&self, price: Cents, qty: i64) -> Cents {
        let numer = 7 * qty * price * (100 - price) * self.multiplier_numer;
        let denom = 10_000 * self.multiplier_denom;
        (numer + denom - 1) / denom
    }
}

/// Placeholder Polymarket schedule, and it now understates.
///
/// The venue charges takers `size * rate * p * (1 - p)`, with `rate` set per
/// market and exposed as Gamma's `feeType`: `crypto_fees_v2` is 0.07,
/// `sports_fees_v3` 0.05, `politics_fees` and finance 0.04, `zero_fees` nothing.
/// Live crypto markets — the ones a BTC ladder would pair against — report
/// `feesEnabled: true`, so at 50c a Crypto leg costs `0.07 * 0.25`, about 1.75%
/// of the dollar it settles for.
///
/// `base_fee_bps` still defaults to zero, which means a cross-venue candidate is
/// priced on the Kalshi leg alone and its edge is *overstated*. That is the
/// dangerous direction, and it is the reason the real model is the next piece of
/// work rather than a later one. It is left at zero rather than guessed because
/// the rate is per market, and a single wrong non-zero figure applied to every
/// market would be indistinguishable from a modelling bug once the real schedule
/// lands. Nothing trades Polymarket yet, so nothing acts on it today.
#[derive(Clone, Default)]
pub struct PolymarketFees {
    pub base_fee_bps: i64,
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
    fn taker_fee(&self, price: Cents, qty: i64) -> Cents {
        let tail = if price < 100 - price {
            price
        } else {
            100 - price
        };
        let numer = self.base_fee_bps * qty * tail * 2;
        let denom = 10_000;
        (numer + denom - 1) / denom
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
        assert_eq!(fees.taker_fee(4, 100), 27);
        assert_eq!(fees.taker_fee(62, 100), 165);
        assert_eq!(fees.taker_fee(29, 100), 145);
        assert_eq!(fees.taker_fee(3, 100), 21);
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
                    let fee = model.taker_fee(price, qty);
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
                    assert_eq!(fee, model.taker_fee(100 - price, qty));
                }
            }
        }
        assert_eq!(KalshiFees::default().taker_fee(0, 100), 0);
        assert_eq!(KalshiFees::default().taker_fee(100, 100), 0);
    }
}
