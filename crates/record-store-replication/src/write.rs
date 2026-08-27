//! The distributed write path.
//!
//! One pass over the client's bytes feeds every selected replica. Nothing is
//! buffered beyond a bounded per-target queue, a slow destination slows the
//! source rather than growing memory, and no metadata is committed until the
//! configured number of replicas has independently verified what it stored.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use futures_util::StreamExt;
use md5::Md5;
use record_store_cluster::{PlacementPlan, PlacementTarget};
use record_store_core::{Checksum, ETag, NodeId, ObjectId};
use record_store_rpc::{ReplicaTarget, TransferExpectation};
use record_store_storage::{
    ReplicaCommitment, StorageError, UploadStream, WriteReplicaRequest, upload_stream,
};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, warn};

use crate::context::ClusterContext;

/// Bounds on one fan-out write.
#[derive(Debug, Clone, Copy)]
pub struct WriteSettings {
    /// Chunks queued per destination before the source is slowed.
    pub queue_depth: usize,
    /// Time one destination may take to accept a chunk before it is dropped.
    pub chunk_timeout: Duration,
}

impl Default for WriteSettings {
    fn default() -> Self {
        Self {
            queue_depth: 8,
            chunk_timeout: Duration::from_secs(30),
        }
    }
}

/// What one fan-out write achieved.
#[derive(Debug, Clone)]
pub struct ReplicationOutcome {
    /// Logical bytes written.
    pub size: u64,
    /// Logical payload checksum computed at the ingress node.
    pub checksum: Checksum,
    /// S3-compatible entity tag computed at the ingress node.
    pub etag: ETag,
    /// Nodes that verified and published the replica.
    pub durable: Vec<NodeId>,
    /// Nodes that failed, with the reason.
    pub failed: Vec<(NodeId, String)>,
}

impl ReplicationOutcome {
    /// Returns how many replicas became durable.
    #[must_use]
    pub fn acknowledgements(&self) -> u8 {
        u8::try_from(self.durable.len()).unwrap_or(u8::MAX)
    }

    /// Returns a compact per-target explanation for operators.
    #[must_use]
    pub fn detail(&self) -> String {
        if self.failed.is_empty() {
            return "all selected replicas succeeded".to_owned();
        }
        self.failed
            .iter()
            .map(|(node, reason)| format!("{node}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

enum Destination {
    Local,
    Remote(ReplicaTarget),
}

struct Channel {
    node_id: NodeId,
    data: Option<mpsc::Sender<Result<Bytes, std::io::Error>>>,
    commitment: Option<oneshot::Sender<Result<ReplicaCommitment, String>>>,
    remote_commitment: Option<oneshot::Sender<Result<(u64, Checksum), String>>>,
    failure: Option<String>,
}

/// Streams one payload to every planned replica in a single pass.
///
/// The operation identity makes a retried transfer recognizable, so replaying a
/// write never produces a second logical replica.
pub async fn replicate(
    context: &ClusterContext,
    object_id: ObjectId,
    operation_id: &str,
    plan: &PlacementPlan,
    mut body: UploadStream,
    settings: WriteSettings,
) -> Result<ReplicationOutcome, StorageError> {
    let mut channels: Vec<Channel> = Vec::with_capacity(plan.targets.len());
    let mut writers = Vec::with_capacity(plan.targets.len());

    for target in &plan.targets {
        let destination = destination_for(context, target)?;
        let (data_sender, data_receiver) = mpsc::channel(settings.queue_depth);
        let stream = ReceiverStream::new(data_receiver);
        match destination {
            Destination::Local => {
                let (commit_sender, commit_receiver) = oneshot::channel();
                let local = Arc::clone(&context.local);
                let operation = operation_id.to_owned();
                let node_id = target.node_id;
                writers.push(tokio::spawn(async move {
                    let result = local
                        .write_replica(WriteReplicaRequest::trailing(
                            operation,
                            object_id,
                            commit_receiver,
                            upload_stream(stream),
                        ))
                        .await;
                    (
                        node_id,
                        result
                            .map(|written| written.checksum)
                            .map_err(|error| error.to_string()),
                    )
                }));
                channels.push(Channel {
                    node_id: target.node_id,
                    data: Some(data_sender),
                    commitment: Some(commit_sender),
                    remote_commitment: None,
                    failure: None,
                });
            }
            Destination::Remote(remote) => {
                let (commit_sender, commit_receiver) = oneshot::channel();
                let transport = Arc::clone(&context.transport);
                let operation = operation_id.to_owned();
                let node_id = target.node_id;
                writers.push(tokio::spawn(async move {
                    let result = transport
                        .write_replica(
                            &remote,
                            &operation,
                            object_id,
                            TransferExpectation::Trailing(commit_receiver),
                            Box::pin(stream),
                        )
                        .await;
                    (
                        node_id,
                        result
                            .map(|written| written.checksum)
                            .map_err(|error| error.to_string()),
                    )
                }));
                channels.push(Channel {
                    node_id: target.node_id,
                    data: Some(data_sender),
                    commitment: None,
                    remote_commitment: Some(commit_sender),
                    failure: None,
                });
            }
        }
    }

    let mut strong = Sha256::new();
    let mut weak = Md5::new();
    let mut size = 0_u64;
    let mut source_failure: Option<String> = None;

    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                source_failure = Some(error.to_string());
                break;
            }
        };
        if chunk.is_empty() {
            continue;
        }
        size = match size.checked_add(chunk.len() as u64) {
            Some(size) => size,
            None => {
                source_failure = Some("object exceeds the addressable size".to_owned());
                break;
            }
        };
        strong.update(&chunk);
        weak.update(&chunk);
        for channel in &mut channels {
            let Some(sender) = channel.data.as_ref() else {
                continue;
            };
            // A bounded queue with a deadline is what turns a stalled
            // destination into a failed target instead of unbounded buffering.
            match tokio::time::timeout(settings.chunk_timeout, sender.send(Ok(chunk.clone()))).await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    channel.failure = Some("replica closed the transfer".to_owned());
                    channel.data = None;
                }
                Err(_) => {
                    warn!(node = %channel.node_id, "replica transfer stalled and was dropped");
                    channel.failure = Some("replica did not accept data in time".to_owned());
                    channel.data = None;
                }
            }
        }
        if channels.iter().all(|channel| channel.data.is_none()) {
            source_failure = Some("every selected replica stopped accepting data".to_owned());
            break;
        }
    }

    let checksum = Checksum::sha256(strong.finalize().into());
    let etag = ETag::from_md5(weak.finalize().into());

    for channel in &mut channels {
        // Closing the data channel ends the destination's stream; the
        // commitment then tells it what the bytes were supposed to be.
        channel.data = None;
        let outcome = match &source_failure {
            Some(reason) => Err(reason.clone()),
            None => Ok((size, checksum.clone())),
        };
        if let Some(sender) = channel.commitment.take() {
            let _ = sender.send(
                outcome
                    .clone()
                    .map(|(size, checksum)| ReplicaCommitment { size, checksum }),
            );
        }
        if let Some(sender) = channel.remote_commitment.take() {
            let _ = sender.send(outcome);
        }
    }

    let mut durable = Vec::new();
    let mut failed = Vec::new();
    for writer in writers {
        match writer.await {
            Ok((node_id, Ok(reported))) => {
                if reported == checksum {
                    durable.push(node_id);
                } else {
                    // A replica that reports a different checksum did not store
                    // what the client sent, so it is not counted as durable.
                    failed.push((
                        node_id,
                        format!("replica reported checksum {reported} instead of {checksum}"),
                    ));
                }
            }
            Ok((node_id, Err(reason))) => failed.push((node_id, reason)),
            Err(error) => failed.push((NodeId::from_uuid(uuid::Uuid::nil()), error.to_string())),
        }
    }
    for channel in channels {
        if let Some(reason) = channel.failure
            && !failed.iter().any(|(node, _)| *node == channel.node_id)
            && !durable.contains(&channel.node_id)
        {
            failed.push((channel.node_id, reason));
        }
    }

    if let Some(reason) = source_failure {
        // The upload itself failed, so nothing was published anywhere. Any
        // staged bytes are cleaned up by the destinations themselves.
        rollback(context, object_id, &durable).await;
        return Err(StorageError::UploadStream(std::io::Error::other(reason)));
    }

    debug!(
        %object_id,
        durable = durable.len(),
        failed = failed.len(),
        "replicated payload"
    );
    Ok(ReplicationOutcome {
        size,
        checksum,
        etag,
        durable,
        failed,
    })
}

fn destination_for(
    context: &ClusterContext,
    target: &PlacementTarget,
) -> Result<Destination, StorageError> {
    if target.node_id == context.node_id {
        Ok(Destination::Local)
    } else if target.rpc_address.is_empty() {
        Err(StorageError::ClusterUnavailable(format!(
            "node {} has no internal address",
            target.node_id
        )))
    } else {
        Ok(Destination::Remote(ReplicaTarget {
            node_id: target.node_id,
            address: target.rpc_address.clone(),
        }))
    }
}

/// Removes replicas written for a write that will not be committed.
///
/// This is best effort: anything left behind is unreferenced by metadata and is
/// collected by the garbage collector, which is why the collector is
/// conservative about what it considers garbage.
pub async fn rollback(context: &ClusterContext, object_id: ObjectId, nodes: &[NodeId]) {
    for node_id in nodes {
        if *node_id == context.node_id {
            if let Err(error) = context.local.delete_replica(object_id).await {
                warn!(%object_id, %error, "could not release a local uncommitted replica");
            }
            continue;
        }
        match context.target(*node_id).await {
            Ok(target) => {
                if let Err(error) = context.transport.delete_replica(&target, object_id).await {
                    warn!(
                        %object_id,
                        node = %node_id,
                        %error,
                        "could not release an uncommitted replica; the collector will retry"
                    );
                }
            }
            Err(error) => {
                warn!(%object_id, node = %node_id, %error, "could not resolve a replica holder");
            }
        }
    }
}
