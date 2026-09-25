//! Moving the storage events a committed mutation owes into the outbox.
//!
//! The catalog records the intent to publish inside the transaction that
//! commits the mutation, so the two become durable together and no crash can
//! produce one without the other. That leaves a second, separate problem: the
//! journal has to be drained into the outbox the webhook worker delivers from.
//!
//! The handover is deliberately not atomic — the catalog and the outbox are
//! different databases, and binding them into one transaction would mean
//! putting subscriber state inside the authoritative catalog. What makes it
//! safe anyway is that the outbox records how far it has drained *in the same
//! transaction* that inserts the events and enqueues their deliveries. A crash
//! mid-handover therefore either committed both or neither, and the next pass
//! resumes from the recorded position. Nothing is published twice by a
//! restart, and nothing is lost by one.
//!
//! ## What this does not make exactly-once
//!
//! Delivery to a subscriber is at-least-once and always will be: an endpoint
//! that receives a webhook and then fails to answer will be retried. Events
//! carry a stable identifier, allocated when the mutation committed, so a
//! subscriber can discard a repeat.
//!
//! In a deployment with several nodes the outbox is node-local while the
//! journal is replicated. One node drains at a time, which the activation gate
//! enforces; if that node is replaced, the node taking over resumes from *its*
//! outbox position, which may be behind. Events can therefore be delivered a
//! second time across a handover. That is a consequence of the outbox being
//! node-local and is stated rather than papered over.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use record_store_events::EventRepository;
use record_store_metadata::MetadataRepository;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::ServiceError;

/// Decides whether this process may currently drain the journal.
///
/// Draining from several processes at once would publish each event into
/// several node-local outboxes and deliver it several times. In a single-node
/// deployment there is nothing to decide and no gate is installed.
#[async_trait]
pub trait EventPumpGate: Send + Sync {
    /// Returns whether draining is permitted right now.
    async fn active(&self) -> bool;
}

/// Drains the catalog's storage-event journal into the delivery outbox.
pub struct StorageEventPump {
    metadata: Arc<dyn MetadataRepository>,
    events: Arc<dyn EventRepository>,
    batch: usize,
    interval: Duration,
    gate: Option<Arc<dyn EventPumpGate>>,
    /// How far the journal has been pruned by this process.
    ///
    /// Only an optimisation: pruning is idempotent, and this stops a quiet
    /// deployment from writing a prune command on every tick.
    pruned_through: AtomicU64,
}

impl StorageEventPump {
    /// Creates a pump over one catalog and one outbox.
    #[must_use]
    pub fn new(
        metadata: Arc<dyn MetadataRepository>,
        events: Arc<dyn EventRepository>,
        interval: Duration,
    ) -> Self {
        Self {
            metadata,
            events,
            batch: 500,
            interval,
            gate: None,
            pruned_through: AtomicU64::new(0),
        }
    }

    /// Restricts draining to when the gate allows it.
    #[must_use]
    pub fn with_activation_gate(mut self, gate: Arc<dyn EventPumpGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Sets how many journalled events one pass moves.
    #[must_use]
    pub const fn with_batch_size(mut self, batch: usize) -> Self {
        self.batch = batch;
        self
    }

    /// Moves one bounded batch, returning how many events were taken.
    pub async fn run_once(&self) -> Result<usize, ServiceError> {
        let watermark = self
            .events
            .drained_through()
            .await
            .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
        let pending = self
            .metadata
            .pending_mutation_events(watermark, self.batch)
            .await?;
        if pending.is_empty() {
            // A crash between the outbox commit and the prune leaves rows the
            // outbox has already taken. They are invisible to the query above,
            // so they are cleared here rather than kept forever.
            self.prune_to(watermark).await;
            return Ok(0);
        }
        let taken = pending.len();
        let through = self
            .events
            .drain_journal(&pending)
            .await
            .map_err(|error| ServiceError::InvalidRequest(error.to_string()))?;
        self.prune_to(through).await;
        Ok(taken)
    }

    /// Clears journal rows the outbox has taken.
    ///
    /// A failure here is not a failure of the pass: the events are already in
    /// the outbox, and the rows will be cleared on a later one. Treating it as
    /// fatal would stop delivery over a housekeeping problem.
    async fn prune_to(&self, through: u64) {
        if through == 0 || self.pruned_through.load(Ordering::Relaxed) >= through {
            return;
        }
        match self.metadata.prune_mutation_events(through).await {
            Ok(()) => {
                self.pruned_through.store(through, Ordering::Relaxed);
            }
            Err(error) => {
                warn!(%error, through, "journalled events were delivered but not yet pruned");
            }
        }
    }

    /// Runs until cancellation, draining on an interval.
    pub async fn run(self, cancellation: CancellationToken) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        info!("storage event pump started");
        loop {
            tokio::select! {
                () = cancellation.cancelled() => {
                    info!("storage event pump stopped");
                    return;
                }
                _ = interval.tick() => {
                    if let Some(gate) = &self.gate
                        && !gate.active().await
                    {
                        continue;
                    }
                    // Keep going while the journal is behind, so a burst is
                    // drained at the rate it was written rather than at one
                    // batch per tick.
                    loop {
                        match self.run_once().await {
                            Ok(0) => break,
                            Ok(moved) => {
                                info!(moved, "storage events moved into the delivery outbox");
                                if moved < self.batch {
                                    break;
                                }
                            }
                            Err(error) => {
                                error!(%error, "storage event pump pass failed");
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}
