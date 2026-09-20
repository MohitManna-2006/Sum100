use crate::types::Venue;
use serde::Serialize;

/// Upper bounds of the latency histogram buckets, in milliseconds.
///
/// A sample falls in the first bucket whose bound it does not exceed; anything
/// slower than the last bound lands in the overflow bucket, which is why
/// [`Metrics::latency_buckets`] holds one more slot than there are bounds.
pub const LATENCY_BUCKET_BOUNDS_MS: [u64; 6] = [1, 5, 10, 50, 100, 500];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// Quotes finer than one cent, which this book cannot hold.
    ///
    /// Counted apart from [`Metrics::parse_errors`] because it is not a parse
    /// failure: the payload was understood perfectly, and the price is one the
    /// engine's whole-cent book has no slot for. Conflating them reports a
    /// venue quoting a finer tick as a venue sending malformed data, and the
    /// two call for completely different responses.
    pub sub_cent_prices: u64,
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
        let index = LATENCY_BUCKET_BOUNDS_MS
            .iter()
            .position(|bound| latency <= *bound)
            .unwrap_or(LATENCY_BUCKET_BOUNDS_MS.len());
        self.latency_buckets[index] += 1;
    }

    /// Latency samples taken, which is deltas seen minus those the venue
    /// stamped in the future.
    pub fn latency_samples(&self) -> u64 {
        self.latency_buckets.iter().sum()
    }

    /// Bucketed quantile estimate, in milliseconds.
    ///
    /// A histogram cannot name the sample sitting at a quantile, only the bucket
    /// it landed in, so this reports that bucket's upper bound: a figure the
    /// true latency is at or below. The overflow bucket has no upper bound and
    /// reports the last finite one instead, which is the usual convention and
    /// the only finite answer available — the dashboard's own validator rejects
    /// a non-finite number outright, and understating a quantile that is already
    /// off the top of the scale is less misleading than dropping the frame.
    ///
    /// Zero samples reports zero, which reads as "nothing measured" beside a
    /// message count the dashboard shows next to it.
    pub fn latency_percentile_ms(&self, quantile: f64) -> f64 {
        let total = self.latency_samples();
        if total == 0 {
            return 0.0;
        }
        let target = (quantile * total as f64).ceil().max(1.0);
        let last = LATENCY_BUCKET_BOUNDS_MS.len() - 1;
        let mut cumulative = 0u64;
        for (bucket, count) in self.latency_buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative as f64 >= target {
                return LATENCY_BUCKET_BOUNDS_MS[bucket.min(last)] as f64;
            }
        }
        LATENCY_BUCKET_BOUNDS_MS[last] as f64
    }

    pub fn log(&self, venue: Venue) {
        tracing::info!(?venue, ?self, "feed metrics");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each quantile names the bucket its own sample landed in, reporting that
    /// bucket's ceiling. A fast feed with a slow tail is the case that matters:
    /// the median must stay fast while p99 shows the tail, because a single
    /// figure averaging the two would hide exactly the latency worth seeing.
    #[test]
    fn percentiles_name_the_bucket_the_quantile_lands_in() {
        let mut metrics = Metrics::default();
        for (count, latency) in [(90, 3), (5, 40), (5, 400)] {
            for _ in 0..count {
                metrics.observe_latency(latency, 0);
            }
        }
        assert_eq!(metrics.latency_samples(), 100);
        assert_eq!(metrics.latency_percentile_ms(0.50), 5.0);
        assert_eq!(metrics.latency_percentile_ms(0.95), 50.0);
        assert_eq!(metrics.latency_percentile_ms(0.99), 500.0);
    }

    /// The tail bucket is unbounded above, so a quantile inside it reports the
    /// last finite bound. Understating is the only finite option; it must never
    /// report zero, which would read as a perfectly fast feed.
    #[test]
    fn the_overflow_bucket_reports_the_last_finite_bound() {
        let mut metrics = Metrics::default();
        metrics.observe_latency(9_000, 0);
        assert_eq!(metrics.latency_buckets[6], 1);
        assert_eq!(metrics.latency_percentile_ms(0.5), 500.0);
    }

    /// Nothing measured is zero, and it must not be confused with a real
    /// sub-millisecond reading: the sample count is what tells them apart.
    #[test]
    fn no_samples_is_zero_and_says_so_in_the_sample_count() {
        let metrics = Metrics::default();
        assert_eq!(metrics.latency_samples(), 0);
        assert_eq!(metrics.latency_percentile_ms(0.5), 0.0);

        let mut fast = Metrics::default();
        fast.observe_latency(1, 0);
        assert_eq!(fast.latency_samples(), 1);
        assert_eq!(fast.latency_percentile_ms(0.5), 1.0);
    }

    /// A venue timestamp ahead of local receipt is clock skew, not negative
    /// latency, and must stay out of the histogram entirely.
    #[test]
    fn clock_skew_is_counted_separately_and_never_enters_the_histogram() {
        let mut metrics = Metrics::default();
        metrics.observe_latency(100, 500);
        assert_eq!(metrics.clock_skew_samples, 1);
        assert_eq!(metrics.latency_samples(), 0);
    }

    /// The bucket a sample lands in is decided by the shared bound table, so the
    /// boundaries are inclusive and the table is the only place they are stated.
    #[test]
    fn bucket_edges_are_inclusive_and_come_from_the_shared_table() {
        for (index, bound) in LATENCY_BUCKET_BOUNDS_MS.iter().enumerate() {
            let mut metrics = Metrics::default();
            metrics.observe_latency(*bound, 0);
            assert_eq!(metrics.latency_buckets[index], 1, "bound {bound} ms");

            let mut over = Metrics::default();
            over.observe_latency(bound + 1, 0);
            assert_eq!(over.latency_buckets[index], 0, "bound {bound} ms");
        }
    }
}
