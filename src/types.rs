pub type Cents = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    Kalshi,
    Polymarket,
}

#[derive(Debug, Clone, Copy)]
pub struct Level {
    pub price: Cents,
    pub size: i64,
}

#[derive(Debug, Clone)]
pub struct Book {
    pub venue: Venue,
    pub contract_id: u32,
    pub asks: Vec<Level>,
    pub seq: u64,
    pub updated_at_ms: u64,
}

impl Book {
    pub fn is_fresh(&self, now_ms: u64, max_age_ms: u64) -> bool {
        now_ms.saturating_sub(self.updated_at_ms) <= max_age_ms
    }
}
