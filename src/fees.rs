use crate::types::Cents;

pub trait FeeModel {
    fn taker_fee(&self, price: Cents, qty: i64) -> Cents;
}

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

pub struct PolymarketFees {
    pub base_fee_bps: i64,
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
