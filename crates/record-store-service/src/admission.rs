//! Admission control: how many operations run at once, and what happens to the
//! ones that cannot start.
//!
//! A concurrency limit on its own is only half an answer. It bounds the work in
//! flight, but every request beyond the limit still queues — holding a
//! connection, a task, and whatever buffers came with it — for as long as the
//! client is willing to wait. Under sustained overload that queue is the thing
//! that exhausts memory, and it does so invisibly, because nothing counts it.
//!
//! So admission here is explicit: a request waits for a permit up to a bounded
//! time and is then refused with a retryable error rather than queued forever.
//! The numbers an operator needs to see that happening — how many operations
//! are running, how many are waiting, how many were refused — are counted at
//! the same place the decision is made.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{ServiceError, metrics::ServiceMetrics};

/// The shared operation budget and the policy for waiting on it.
pub(crate) struct Admission {
    operations: Arc<Semaphore>,
    metrics: Arc<ServiceMetrics>,
    /// How long a request may wait for a permit before it is refused.
    wait_limit: Duration,
}

impl Admission {
    pub(crate) fn new(limit: usize, wait_limit: Duration, metrics: Arc<ServiceMetrics>) -> Self {
        metrics
            .concurrency_limit
            .store(limit as u64, Ordering::Relaxed);
        Self {
            operations: Arc::new(Semaphore::new(limit)),
            metrics,
            wait_limit,
        }
    }

    /// Takes a permit, or refuses the operation.
    ///
    /// The refusal is deliberately a distinct error rather than a generic
    /// failure: a caller that is told "too busy, try again" can back off, and a
    /// caller that is told "internal error" cannot.
    pub(crate) async fn acquire(&self) -> Result<OperationPermit, ServiceError> {
        self.metrics
            .queued_operations
            .fetch_add(1, Ordering::Relaxed);
        let waited = tokio::time::timeout(
            self.wait_limit,
            Arc::clone(&self.operations).acquire_owned(),
        )
        .await;
        self.metrics
            .queued_operations
            .fetch_sub(1, Ordering::Relaxed);

        match waited {
            Ok(Ok(permit)) => {
                self.metrics
                    .active_operations
                    .fetch_add(1, Ordering::Relaxed);
                Ok(OperationPermit {
                    _permit: permit,
                    metrics: Arc::clone(&self.metrics),
                })
            }
            // The semaphore is only ever closed on shutdown.
            Ok(Err(_)) => Err(ServiceError::Unavailable),
            Err(_elapsed) => {
                self.metrics
                    .rejected_operations
                    .fetch_add(1, Ordering::Relaxed);
                Err(ServiceError::Overloaded)
            }
        }
    }
}

/// A permit that keeps the active-operation count honest.
///
/// Counting admissions without counting departures would produce a gauge that
/// only goes up, which is worse than no gauge at all. Tying the decrement to
/// the permit's own lifetime means cancellation, early return, and panic all
/// release it the same way.
pub(crate) struct OperationPermit {
    _permit: OwnedSemaphorePermit,
    metrics: Arc<ServiceMetrics>,
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        self.metrics
            .active_operations
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(limit: usize, wait: Duration) -> (Admission, Arc<ServiceMetrics>) {
        let metrics = Arc::new(ServiceMetrics::default());
        (Admission::new(limit, wait, Arc::clone(&metrics)), metrics)
    }

    /// The gauge has to come back down, or an operator watching it would see a
    /// deployment that appears permanently saturated.
    #[tokio::test]
    async fn a_released_permit_lowers_the_active_count() {
        let (admission, metrics) = admission(2, Duration::from_secs(5));
        let first = admission.acquire().await.expect("a permit is available");
        assert_eq!(metrics.snapshot().active_operations, 1);
        let second = admission.acquire().await.expect("a second permit");
        assert_eq!(metrics.snapshot().active_operations, 2);
        drop(first);
        assert_eq!(metrics.snapshot().active_operations, 1);
        drop(second);
        assert_eq!(metrics.snapshot().active_operations, 0);
    }

    /// Overload has to be refused, not absorbed. Before this, a request beyond
    /// the limit waited for as long as the client held on, so the queue — not
    /// the limit — decided how much memory the process used.
    #[tokio::test]
    async fn work_beyond_the_limit_is_refused_rather_than_queued_forever() {
        let (admission, metrics) = admission(1, Duration::from_millis(50));
        let _held = admission.acquire().await.expect("the only permit");

        let Err(refused) = admission.acquire().await else {
            panic!("the second operation cannot start while the first holds the permit");
        };
        assert!(
            matches!(refused, ServiceError::Overloaded),
            "overload must be its own answer, not a generic failure: {refused}"
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.rejected_operations, 1);
        assert_eq!(snapshot.active_operations, 1, "the holder still holds it");
        assert_eq!(
            snapshot.queued_operations, 0,
            "a refused waiter has left the queue"
        );
    }

    /// A request that waits and then gets in is not a rejection, and must not be
    /// counted as one.
    #[tokio::test]
    async fn a_request_that_waits_briefly_still_succeeds() {
        let (admission, metrics) = admission(1, Duration::from_secs(5));
        let held = admission.acquire().await.expect("the only permit");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
        });
        admission.acquire().await.expect("the permit comes free");
        assert_eq!(metrics.snapshot().rejected_operations, 0);
    }

    /// Queue depth is what tells an operator that a deployment is near its
    /// limit before anything is refused.
    #[tokio::test]
    async fn waiting_work_is_visible_while_it_waits() {
        let metrics = Arc::new(ServiceMetrics::default());
        let admission = Arc::new(Admission::new(
            1,
            Duration::from_secs(5),
            Arc::clone(&metrics),
        ));
        let held = admission.acquire().await.expect("the only permit");

        let waiter = {
            let admission = Arc::clone(&admission);
            tokio::spawn(async move { admission.acquire().await.map(|_| ()) })
        };
        // Let the waiter reach the semaphore.
        for _ in 0..100 {
            if metrics.snapshot().queued_operations == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            metrics.snapshot().queued_operations,
            1,
            "a waiting operation has to be countable"
        );
        drop(held);
        waiter.await.expect("task").expect("the waiter gets in");
        assert_eq!(metrics.snapshot().queued_operations, 0);
    }

    /// The limit is reported, because a saturation gauge is meaningless without
    /// the number it is saturating against.
    #[tokio::test]
    async fn the_configured_limit_is_reported() {
        let (_admission, metrics) = admission(37, Duration::from_secs(1));
        assert_eq!(metrics.snapshot().concurrency_limit, 37);
    }
}
