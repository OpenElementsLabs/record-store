//! Shared bucket and object application services.

use std::sync::atomic::{AtomicU64, Ordering};

/// Shared service-layer operation metrics without high-cardinality labels.
#[derive(Debug, Default)]
pub struct ServiceMetrics {
    pub(crate) requests: AtomicU64,
    pub(crate) errors: AtomicU64,
    pub(crate) upload_bytes: AtomicU64,
    pub(crate) download_bytes: AtomicU64,
    /// Operations holding a permit right now.
    pub(crate) active_operations: AtomicU64,
    /// Operations waiting for one.
    pub(crate) queued_operations: AtomicU64,
    /// Operations refused because they waited too long for one.
    pub(crate) rejected_operations: AtomicU64,
    /// The configured ceiling, so saturation can be read off the gauges.
    pub(crate) concurrency_limit: AtomicU64,
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
            active_operations: self.active_operations.load(Ordering::Relaxed),
            queued_operations: self.queued_operations.load(Ordering::Relaxed),
            rejected_operations: self.rejected_operations.load(Ordering::Relaxed),
            concurrency_limit: self.concurrency_limit.load(Ordering::Relaxed),
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
    /// Operations holding an admission permit at the moment of the snapshot.
    pub active_operations: u64,
    /// Operations waiting for a permit at that moment.
    pub queued_operations: u64,
    /// Operations refused since start-up because they waited too long.
    pub rejected_operations: u64,
    /// The configured concurrency ceiling.
    pub concurrency_limit: u64,
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
                active_operations: 0,
                queued_operations: 0,
                rejected_operations: 0,
                concurrency_limit: 0,
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
                active_operations: 0,
                queued_operations: 0,
                rejected_operations: 0,
                concurrency_limit: 0,
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
