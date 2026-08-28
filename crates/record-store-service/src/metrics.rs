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
