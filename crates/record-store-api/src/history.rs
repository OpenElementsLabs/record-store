//! A short, bounded history of the service counters.
//!
//! Record Store exposes counters, not rates, and a rate can only come from
//! comparing two readings. The console used to do all of that comparing itself,
//! which meant it could only ever show a rate it had personally watched happen:
//! nothing on the first paint, a single point after the second poll, and a trend
//! that took minutes to fill and was thrown away by a page reload.
//!
//! Sampling here instead moves the waiting off the person looking at the screen.
//! The server has been reading its own counters since it started, so the first
//! request for the page can be answered with a populated chart.
//!
//! This is deliberately in memory and deliberately small. It exists to draw a
//! graph, and a graph is not a record — paying redb writes and a pruning policy
//! for it would be spending durability on the one kind of data that does not
//! need any. A restart starts the window again, which the endpoint reports
//! rather than hides.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use record_store_service::ServiceMetricsSnapshot;
use serde::{Deserialize, Serialize};

/// Seconds between samples.
///
/// Matches the console's own polling cadence, so a seeded window and the samples
/// the console goes on to take itself are evenly spaced rather than stitched
/// together at two different resolutions.
pub const SAMPLE_INTERVAL_SECONDS: u64 = 15;

/// Samples retained: one hour at [`SAMPLE_INTERVAL_SECONDS`].
///
/// Each sample is five `u64`s and a timestamp, so the whole ring is a few tens
/// of kilobytes. The bound is what makes it safe to keep on a busy node forever.
pub const SAMPLE_CAPACITY: usize = 240;

/// One reading of the counters.
///
/// Counters, not rates. A consumer differentiates consecutive samples, which is
/// the same thing a Prometheus scraper does, and means this endpoint never has
/// to guess what window the caller wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsSample {
    /// When the reading was taken.
    pub at: DateTime<Utc>,
    /// Total service operations started.
    pub requests: u64,
    /// Total service operations that returned an error.
    pub errors: u64,
    /// Bytes committed through uploads.
    pub upload_bytes: u64,
    /// Bytes yielded through downloads.
    pub download_bytes: u64,
}

impl MetricsSample {
    /// Builds a sample from a counter snapshot taken at `at`.
    #[must_use]
    pub const fn new(at: DateTime<Utc>, metrics: ServiceMetricsSnapshot) -> Self {
        Self {
            at,
            requests: metrics.requests,
            errors: metrics.errors,
            upload_bytes: metrics.upload_bytes,
            download_bytes: metrics.download_bytes,
        }
    }
}

/// What the history endpoint returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsHistoryResponse {
    /// Nominal seconds between samples. Real spacing can differ after a pause,
    /// so a consumer should use each sample's own timestamp to compute a rate.
    pub interval_seconds: u64,
    /// How many samples the ring holds at most.
    pub capacity: usize,
    /// When this process started sampling. A window shorter than the retention
    /// means the server restarted, not that traffic stopped.
    pub started_at: DateTime<Utc>,
    /// Samples, oldest first.
    pub samples: Vec<MetricsSample>,
}

/// A bounded ring of counter readings.
#[derive(Debug)]
pub struct MetricsHistory {
    samples: Mutex<VecDeque<MetricsSample>>,
    started_at: DateTime<Utc>,
    capacity: usize,
    interval_seconds: u64,
}

impl MetricsHistory {
    /// Creates a history with the default cadence and retention.
    #[must_use]
    pub fn new(started_at: DateTime<Utc>) -> Self {
        Self::with_capacity(started_at, SAMPLE_CAPACITY, SAMPLE_INTERVAL_SECONDS)
    }

    /// Creates a history with an explicit bound, for tests.
    #[must_use]
    pub fn with_capacity(
        started_at: DateTime<Utc>,
        capacity: usize,
        interval_seconds: u64,
    ) -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity.min(SAMPLE_CAPACITY))),
            started_at,
            capacity: capacity.max(1),
            interval_seconds,
        }
    }

    /// Records one reading, dropping the oldest once the ring is full.
    ///
    /// A reading at or before the newest one is ignored. Counters only move
    /// forward, so an out-of-order sample would make a consumer compute a
    /// negative rate from what is really a clock or scheduling artefact.
    pub fn record(&self, sample: MetricsSample) {
        let Ok(mut samples) = self.samples.lock() else {
            // A poisoned lock means a previous sampler panicked mid-write. The
            // graph is not worth propagating that into the request path.
            return;
        };
        if samples.back().is_some_and(|latest| latest.at >= sample.at) {
            return;
        }
        if samples.len() == self.capacity {
            samples.pop_front();
        }
        samples.push_back(sample);
    }

    /// Returns every retained sample, oldest first.
    #[must_use]
    pub fn response(&self) -> MetricsHistoryResponse {
        let samples = self
            .samples
            .lock()
            .map(|samples| samples.iter().copied().collect())
            .unwrap_or_default();
        MetricsHistoryResponse {
            interval_seconds: self.interval_seconds,
            capacity: self.capacity,
            started_at: self.started_at,
            samples,
        }
    }
}

impl MetricsHistory {
    /// Samples the current counters into the ring.
    pub fn observe(self: &Arc<Self>, at: DateTime<Utc>, metrics: ServiceMetricsSnapshot) {
        self.record(MetricsSample::new(at, metrics));
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn counters(requests: u64) -> ServiceMetricsSnapshot {
        ServiceMetricsSnapshot {
            requests,
            errors: 0,
            upload_bytes: requests * 10,
            download_bytes: requests * 5,
            active_operations: 0,
            queued_operations: 0,
            rejected_operations: 0,
            concurrency_limit: 0,
        }
    }

    #[test]
    fn samples_are_returned_oldest_first() {
        let start = Utc::now();
        let history = MetricsHistory::new(start);
        for index in 0..5_u64 {
            history.record(MetricsSample::new(
                start + Duration::seconds(index as i64 * 15),
                counters(index),
            ));
        }
        let response = history.response();
        let requests: Vec<u64> = response.samples.iter().map(|s| s.requests).collect();
        assert_eq!(requests, vec![0, 1, 2, 3, 4]);
        assert_eq!(response.interval_seconds, SAMPLE_INTERVAL_SECONDS);
        assert_eq!(response.started_at, start);
    }

    /// The bound is what makes this safe to leave running for months. Without
    /// it a busy node would grow one sample every fifteen seconds forever.
    #[test]
    fn the_ring_never_grows_past_its_capacity() {
        let start = Utc::now();
        let history = MetricsHistory::with_capacity(start, 4, 15);
        for index in 0..100_u64 {
            history.record(MetricsSample::new(
                start + Duration::seconds(index as i64 * 15),
                counters(index),
            ));
        }
        let response = history.response();
        assert_eq!(response.samples.len(), 4);
        // The newest four, not the oldest four.
        let requests: Vec<u64> = response.samples.iter().map(|s| s.requests).collect();
        assert_eq!(requests, vec![96, 97, 98, 99]);
    }

    /// Counters only move forward. A sample that arrives out of order would let
    /// a consumer compute a negative rate from a scheduling artefact.
    #[test]
    fn an_out_of_order_sample_is_ignored() {
        let start = Utc::now();
        let history = MetricsHistory::new(start);
        history.record(MetricsSample::new(
            start + Duration::seconds(30),
            counters(10),
        ));
        history.record(MetricsSample::new(
            start + Duration::seconds(15),
            counters(5),
        ));
        history.record(MetricsSample::new(
            start + Duration::seconds(30),
            counters(7),
        ));

        let response = history.response();
        assert_eq!(response.samples.len(), 1, "only the first reading is kept");
        assert_eq!(response.samples[0].requests, 10);
    }

    #[test]
    fn an_empty_history_still_describes_itself() {
        let start = Utc::now();
        let response = MetricsHistory::new(start).response();
        assert!(response.samples.is_empty());
        assert_eq!(response.capacity, SAMPLE_CAPACITY);
        assert_eq!(response.started_at, start);
    }

    /// One hour of retention at the console's own cadence, so a seeded window
    /// and the samples the console takes itself line up.
    #[test]
    fn the_defaults_cover_one_hour() {
        assert_eq!(SAMPLE_CAPACITY as u64 * SAMPLE_INTERVAL_SECONDS, 3_600);
    }

    #[test]
    fn a_sample_carries_every_counter_the_charts_draw() {
        let at = Utc::now();
        let sample = MetricsSample::new(at, counters(4));
        assert_eq!(sample.at, at);
        assert_eq!(sample.requests, 4);
        assert_eq!(sample.upload_bytes, 40);
        assert_eq!(sample.download_bytes, 20);
        assert_eq!(sample.errors, 0);
    }
}
