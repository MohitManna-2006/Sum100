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
