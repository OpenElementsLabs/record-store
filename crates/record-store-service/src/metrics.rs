//! Shared bucket and object application services.

use std::sync::atomic::{AtomicU64, Ordering};

/// Shared service-layer operation metrics without high-cardinality labels.
#[derive(Debug, Default)]
pub struct ServiceMetrics {
    pub(crate) requests: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) upload_bytes: AtomicU64,
    pub(crate) download_bytes: AtomicU64,
}

impl ServiceMetrics {
    /// Returns a point-in-time metric snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ServiceMetricsSnapshot {
        ServiceMetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            upload_bytes: self.upload_bytes.load(Ordering::Relaxed),
            download_bytes: self.download_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Copyable metrics snapshot for native status and Prometheus exposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceMetricsSnapshot {
    /// Total service operations started.
    pub requests: u64,
    /// Total service operations that returned an error.
    pub errors: u64,
    /// Bytes successfully committed through PUT operations.
    pub upload_bytes: u64,
    /// Bytes yielded through download streams.
    pub download_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_reports_every_counter_independently() {
        let metrics = ServiceMetrics::default();
        assert_eq!(
            metrics.snapshot(),
            ServiceMetricsSnapshot {
                requests: 0,
                errors: 0,
                upload_bytes: 0,
                download_bytes: 0,
            }
        );

        metrics.requests.fetch_add(3, Ordering::Relaxed);
        metrics.errors.fetch_add(1, Ordering::Relaxed);
        metrics.upload_bytes.fetch_add(2_048, Ordering::Relaxed);
        metrics.download_bytes.fetch_add(512, Ordering::Relaxed);

        assert_eq!(
            metrics.snapshot(),
            ServiceMetricsSnapshot {
                requests: 3,
                errors: 1,
                upload_bytes: 2_048,
                download_bytes: 512,
            }
        );
    }

    /// The counters are shared across every in-flight operation, so concurrent
    /// increments have to be additive rather than racing each other away.
    #[test]
    fn concurrent_increments_are_not_lost() {
        let metrics = std::sync::Arc::new(ServiceMetrics::default());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let metrics = std::sync::Arc::clone(&metrics);
                scope.spawn(move || {
                    for _ in 0..1_000 {
                        metrics.requests.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(metrics.snapshot().requests, 8_000);
    }
}
