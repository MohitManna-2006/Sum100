use crate::types::Venue;

#[derive(Debug, Default)]
pub struct Metrics {
    pub messages_received: u64,
    pub parse_errors: u64,
    pub parse_attempts: u64,
    pub unknown_messages: u64,
    pub reconnections: u64,
    /// Uncompressed envelope bytes written, not compressed filesystem size.
    pub bytes_recorded: u64,
    /// Exact hundredths discarded by mathematical floor, including signed deltas.
    pub discarded_size_hundredths: u64,
    /// Snapshot sides whose key the venue omitted, applied as empty sides.
    pub snapshot_sides_absent: u64,
    /// Venue-to-local receipt latency buckets: <=1, 5, 10, 50, 100, 500, >500 ms.
    pub latency_buckets: [u64; 7],
    pub clock_skew_samples: u64,
}
impl Metrics {
    pub fn observe_latency(&mut self, received: u64, venue: u64) {
        if venue > received {
            self.clock_skew_samples += 1;
            return;
        }
        let latency = received - venue;
        let index = [1, 5, 10, 50, 100, 500]
            .iter()
            .position(|v| latency <= *v)
            .unwrap_or(6);
        self.latency_buckets[index] += 1;
    }
    pub fn log(&self, venue: Venue) {
        tracing::info!(?venue, ?self, "feed metrics");
    }
}
