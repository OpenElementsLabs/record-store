//! The distributed read path.
//!
//! Reads prefer a healthy local replica, fall back to other healthy replicas
//! while nothing has been sent to the client yet, verify integrity as bytes
//! flow, and record damage for repair without making the caller wait for it.

use std::sync::Arc;

use futures_util::StreamExt;
use record_store_cluster::{
    ClusterCommand, PayloadPlacement, ReplicaState, ReplicaTaskKind, ReplicaTaskPriority,
};
use record_store_consensus::ClusterWrite;
use record_store_core::{ByteRange, Checksum, NodeId, ObjectId, PayloadFormat, ResolvedByteRange};
use record_store_storage::{DownloadStream, ReadReplicaRequest, StorageError};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::context::ClusterContext;

/// One candidate source for a read, in preference order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadCandidate {
    /// Node holding the replica.
    pub node_id: NodeId,
    /// Whether the replica is on this node.
    pub local: bool,
}

/// Chooses which replicas to read, most preferred first.
///
/// The order is deliberately simple and deterministic: a healthy local replica
/// first so the read never leaves the node, then remote healthy replicas spread
/// by a stable hash so concurrent readers of different objects do not all target
/// the same peer.
#[must_use]
pub fn read_candidates(
    placement: &PayloadPlacement,
    topology: &record_store_cluster::ClusterTopology,
    local_node: NodeId,
) -> Vec<ReadCandidate> {
    let mut candidates: Vec<(bool, [u8; 32], ReadCandidate)> = placement
        .replicas
        .iter()
        .filter(|replica| replica.state == ReplicaState::Healthy)
        .filter(|replica| topology.serves_reads(replica.node_id))
        .map(|replica| {
            let local = replica.node_id == local_node;
            let mut hasher = Sha256::new();
            hasher.update(placement.object_id.as_uuid().as_bytes());
            hasher.update(replica.node_id.as_uuid().as_bytes());
            (
                local,
                hasher.finalize().into(),
                ReadCandidate {
                    node_id: replica.node_id,
                    local,
                },
            )
        })
        .collect();
    candidates.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(left.1.cmp(&right.1))
            .then(left.2.node_id.cmp(&right.2.node_id))
    });
    candidates
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

/// An opened replica read.
pub struct ReplicaRead {
    /// Node the bytes are coming from.
    pub node_id: NodeId,
    /// Resolved range when a partial read was requested.
    pub range: Option<ResolvedByteRange>,
    /// Payload chunks.
    pub body: DownloadStream,
}

/// Opens the best available replica for a payload.
///
/// Failures encountered while opening a replica are recorded for repair and the
/// next candidate is tried. Once a stream is returned, the response has begun:
/// a later failure can only abort the transfer, which is why every candidate is
/// probed before any byte reaches the client.
pub async fn open_replica(
    context: &ClusterContext,
    object_id: ObjectId,
    size: u64,
    checksum: &Checksum,
    payload_format: PayloadFormat,
    range: Option<ByteRange>,
) -> Result<ReplicaRead, StorageError> {
    let placement = context
        .placement_for(object_id)
        .await?
        .ok_or(StorageError::NoHealthyReplica)?;
    let topology = context.topology().await?;
    let candidates = read_candidates(&placement, &topology, context.node_id);
    if candidates.is_empty() {
        record_unreadable(context, &placement).await;
        return Err(StorageError::NoHealthyReplica);
    }

    let mut failures = Vec::new();
    for candidate in candidates {
        let attempt = if candidate.local {
            open_local(context, object_id, size, checksum, payload_format, range).await
        } else {
            open_remote(context, candidate.node_id, object_id, size, checksum, range).await
        };
        match attempt {
            Ok(read) => return Ok(read),
            Err(error) => {
                warn!(
                    %object_id,
                    node = %candidate.node_id,
                    %error,
                    "replica could not be opened; trying the next healthy replica"
                );
                let state = match &error {
                    StorageError::InconsistentState | StorageError::ObjectNotFound => {
                        Some(ReplicaState::Missing)
                    }
                    StorageError::IntegrityMismatch | StorageError::Cryptography => {
                        Some(ReplicaState::Corrupt)
                    }
                    _ => None,
                };
                if let Some(state) = state {
                    report_damage(context, &placement, candidate.node_id, state).await;
                }
                failures.push(format!("{}: {error}", candidate.node_id));
            }
        }
    }
    debug!(%object_id, failures = failures.len(), "no replica could be opened");
    Err(StorageError::NoHealthyReplica)
}

/// Opens a specific replica, used by repair and rebalance transfers.
///
/// Unlike a client read, the caller already knows which replica it wants and
/// what the bytes must hash to, so no candidate selection happens here.
pub async fn open_specific_replica(
    context: &ClusterContext,
    node_id: NodeId,
    object_id: ObjectId,
    size: u64,
    checksum: &Checksum,
    payload_format: PayloadFormat,
) -> Result<ReplicaRead, StorageError> {
    if node_id == context.node_id {
        open_local(context, object_id, size, checksum, payload_format, None).await
    } else {
        open_remote(context, node_id, object_id, size, checksum, None).await
    }
}

async fn open_local(
    context: &ClusterContext,
    object_id: ObjectId,
    size: u64,
    checksum: &Checksum,
    payload_format: PayloadFormat,
    range: Option<ByteRange>,
) -> Result<ReplicaRead, StorageError> {
    let read = context
        .local
        .read_replica(ReadReplicaRequest {
            object_id,
            size,
            payload_format,
            range,
            expected_checksum: range.is_none().then(|| checksum.clone()),
        })
        .await?;
    Ok(ReplicaRead {
        node_id: context.node_id,
        range: read.range,
        body: read.body,
    })
}

async fn open_remote(
    context: &ClusterContext,
    node_id: NodeId,
    object_id: ObjectId,
    size: u64,
    checksum: &Checksum,
    range: Option<ByteRange>,
) -> Result<ReplicaRead, StorageError> {
    if range.is_some() {
        // Ranged reads from a peer are served as a whole-payload stream and
        // sliced locally, which keeps peer-side range handling out of the
        // integrity path. Small ranges of very large objects therefore prefer a
        // local replica, which the candidate ordering already does.
        let full = context
            .transport
            .read_replica(&context.target(node_id).await?, object_id, size, checksum)
            .await
            .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?;
        let resolved = range
            .map(|range| range.resolve(size))
            .transpose()?
            .ok_or(StorageError::InconsistentState)?;
        let body = slice_stream(remote_stream(full, None), resolved);
        return Ok(ReplicaRead {
            node_id,
            range: Some(resolved),
            body,
        });
    }
    let stream = context
        .transport
        .read_replica(&context.target(node_id).await?, object_id, size, checksum)
        .await
        .map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?;
    Ok(ReplicaRead {
        node_id,
        range: None,
        body: remote_stream(stream, Some((size, checksum.clone()))),
    })
}

/// Wraps a peer stream so this node verifies integrity independently.
///
/// The source node verifies its own bytes, but a reader must not depend on that:
/// the bytes could be damaged in transit, and a peer could be compromised. The
/// check is appended after the last chunk, so a mismatch fails the read instead
/// of silently returning damaged content.
fn remote_stream(
    stream: record_store_rpc::RemoteReadStream,
    expectation: Option<(u64, Checksum)>,
) -> DownloadStream {
    use futures_util::{TryStreamExt, stream};

    struct Progress {
        hasher: Sha256,
        read: u64,
        expected: (u64, Checksum),
    }

    let Some(expected) = expectation else {
        return Box::pin(
            stream.map_err(|error| StorageError::ClusterUnavailable(error.to_string())),
        );
    };
    let progress = Arc::new(std::sync::Mutex::new(Some(Progress {
        hasher: Sha256::new(),
        read: 0,
        expected,
    })));
    let finish = Arc::clone(&progress);
    let verified = stream
        .map(move |chunk| {
            let chunk =
                chunk.map_err(|error| StorageError::ClusterUnavailable(error.to_string()))?;
            let mut guard = progress.lock().map_err(|_| StorageError::Coordination)?;
            if let Some(progress) = guard.as_mut() {
                progress.hasher.update(&chunk);
                progress.read = progress.read.saturating_add(chunk.len() as u64);
            }
            Ok(chunk)
        })
        .chain(stream::once(async move {
            let taken = finish
                .lock()
                .map_err(|_| StorageError::Coordination)?
                .take();
            match taken {
                Some(progress) => {
                    let actual = Checksum::sha256(progress.hasher.finalize().into());
                    if actual == progress.expected.1 && progress.read == progress.expected.0 {
                        Ok(bytes::Bytes::new())
                    } else {
                        Err(StorageError::IntegrityMismatch)
                    }
                }
                None => Ok(bytes::Bytes::new()),
            }
        }))
        .try_filter(|chunk| std::future::ready(!chunk.is_empty()));
    Box::pin(verified)
}

fn slice_stream(body: DownloadStream, range: ResolvedByteRange) -> DownloadStream {
    let mut skipped = 0_u64;
    let mut emitted = 0_u64;
    let sliced = body.filter_map(move |chunk| {
        let outcome = match chunk {
            Ok(chunk) => {
                let mut slice = chunk;
                if skipped < range.offset {
                    let skip = (range.offset - skipped).min(slice.len() as u64);
                    skipped += skip;
                    slice = slice.slice(usize::try_from(skip).unwrap_or(usize::MAX)..);
                }
                if emitted >= range.length || slice.is_empty() {
                    None
                } else {
                    let take = (range.length - emitted).min(slice.len() as u64);
                    emitted += take;
                    Some(Ok(
                        slice.slice(..usize::try_from(take).unwrap_or(usize::MAX))
                    ))
                }
            }
            Err(error) => Some(Err(error)),
        };
        std::future::ready(outcome)
    });
    Box::pin(sliced)
}

/// Records that a replica is damaged and queues a repair.
///
/// The caller is not blocked: repair happens in the background so a read stays
/// fast even when the cluster is healing.
pub async fn report_damage(
    context: &ClusterContext,
    placement: &PayloadPlacement,
    node_id: NodeId,
    state: ReplicaState,
) {
    let Some(consensus) = context.consensus.clone() else {
        return;
    };
    let cluster = Arc::clone(&context.cluster);
    let object_id = placement.object_id;
    let size = placement.size;
    let desired = u32::from(placement.desired_replicas);
    let healthy = u32::try_from(
        placement
            .replicas
            .iter()
            .filter(|replica| replica.state == ReplicaState::Healthy && replica.node_id != node_id)
            .count(),
    )
    .unwrap_or(0);
    let kind = if state == ReplicaState::Corrupt {
        ReplicaTaskKind::RepairCorrupt
    } else {
        ReplicaTaskKind::Repair
    };
    tokio::spawn(async move {
        let now = chrono::Utc::now();
        let mark = ClusterWrite::cluster(ClusterCommand::SetReplicaState {
            object_id,
            node_id,
            state,
            checksum: None,
            verified: false,
            at: now,
        });
        if let Err(error) = consensus.write(mark).await {
            warn!(%object_id, %error, "could not record replica damage");
            return;
        }
        let task = record_store_cluster::ReplicaTask::queued(
            object_id,
            kind,
            ReplicaTaskPriority::classify(kind, healthy, desired),
            size,
            now,
        );
        if let Err(error) = consensus
            .write(ClusterWrite::cluster(ClusterCommand::EnqueueTask {
                task: Box::new(task),
            }))
            .await
        {
            warn!(%object_id, %error, "could not queue a repair for a damaged replica");
        }
        let _ = cluster.refresh_durability_counters().await;
    });
}

async fn record_unreadable(context: &ClusterContext, placement: &PayloadPlacement) {
    warn!(
        object = %placement.object_id,
        replicas = placement.replicas.len(),
        "object has no readable replica"
    );
    let _ = context.cluster.refresh_durability_counters().await;
}

#[cfg(test)]
mod tests {
    use record_store_cluster::{NodeState, ReplicaState};
    use record_store_core::{NodeId, ObjectId};

    use super::*;
    use crate::test_support::{node, placement, replica, topology};

    /// Serving a read from anything but a verified replica would hand a client
    /// bytes the cluster knows are wrong or incomplete.
    #[test]
    fn only_healthy_replicas_are_ever_read() {
        let healthy = NodeId::new();
        let others = [
            ReplicaState::Pending,
            ReplicaState::Repairing,
            ReplicaState::Stale,
            ReplicaState::Missing,
            ReplicaState::Deleting,
            ReplicaState::Corrupt,
        ]
        .map(|state| (NodeId::new(), state));

        let mut replicas = vec![replica(healthy, ReplicaState::Healthy)];
        replicas.extend(others.iter().map(|(id, state)| replica(*id, *state)));

        let mut nodes = vec![node(healthy, 1, NodeState::Healthy)];
        nodes.extend(
            others
                .iter()
                .enumerate()
                .map(|(index, (id, _))| node(*id, index as u64 + 2, NodeState::Healthy)),
        );

        let candidates = read_candidates(
            &placement(ObjectId::new(), replicas),
            &topology(nodes),
            NodeId::new(),
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].node_id, healthy);
    }

    /// Reading locally avoids a network hop entirely, so a healthy local replica
    /// must always be tried first.
    #[test]
    fn a_healthy_local_replica_is_always_preferred() {
        let local = NodeId::new();
        let remotes: Vec<NodeId> = (0..4).map(|_| NodeId::new()).collect();

        let mut replicas = vec![replica(local, ReplicaState::Healthy)];
        replicas.extend(remotes.iter().map(|id| replica(*id, ReplicaState::Healthy)));
        let mut nodes = vec![node(local, 1, NodeState::Healthy)];
        nodes.extend(
            remotes
                .iter()
                .enumerate()
                .map(|(index, id)| node(*id, index as u64 + 2, NodeState::Healthy)),
        );

        let candidates = read_candidates(
            &placement(ObjectId::new(), replicas),
            &topology(nodes),
            local,
        );
        assert!(candidates[0].local);
        assert_eq!(candidates[0].node_id, local);
        assert!(
            candidates[1..].iter().all(|candidate| !candidate.local),
            "only one candidate can be local"
        );
    }

    /// A node that has been drained still serves reads while its replicas are
    /// moved away; an unreachable one must be skipped so a read does not stall
    /// on a peer the cluster already knows is gone.
    #[test]
    fn candidacy_follows_whether_the_node_still_serves_reads() {
        for (state, expected) in [
            (NodeState::Healthy, true),
            (NodeState::Suspect, true),
            (NodeState::Draining, true),
            (NodeState::Unreachable, false),
            (NodeState::Joining, false),
            (NodeState::Decommissioned, false),
        ] {
            let holder = NodeId::new();
            let candidates = read_candidates(
                &placement(
                    ObjectId::new(),
                    vec![replica(holder, ReplicaState::Healthy)],
                ),
                &topology(vec![node(holder, 1, state)]),
                NodeId::new(),
            );
            assert_eq!(
                !candidates.is_empty(),
                expected,
                "{state:?} was treated incorrectly"
            );
        }
    }

    /// A replica record for a node the topology has never heard of must not be
    /// read from: there is no address to reach and no health to trust.
    #[test]
    fn a_replica_on_an_unknown_node_is_not_a_candidate() {
        let candidates = read_candidates(
            &placement(
                ObjectId::new(),
                vec![replica(NodeId::new(), ReplicaState::Healthy)],
            ),
            &topology(Vec::new()),
            NodeId::new(),
        );
        assert!(candidates.is_empty());
    }

    /// Ordering has to be a pure function of the object and the node set, so
    /// every reader agrees and a retry does not reshuffle the fallback order.
    #[test]
    fn the_order_is_deterministic_for_the_same_object() {
        let holders: Vec<NodeId> = (0..5).map(|_| NodeId::new()).collect();
        let replicas = holders
            .iter()
            .map(|id| replica(*id, ReplicaState::Healthy))
            .collect();
        let nodes = holders
            .iter()
            .enumerate()
            .map(|(index, id)| node(*id, index as u64 + 1, NodeState::Healthy))
            .collect();
        let placement = placement(ObjectId::new(), replicas);
        let topology = topology(nodes);
        let reader = NodeId::new();

        let first = read_candidates(&placement, &topology, reader);
        let second = read_candidates(&placement, &topology, reader);
        assert_eq!(
            first.iter().map(|c| c.node_id).collect::<Vec<_>>(),
            second.iter().map(|c| c.node_id).collect::<Vec<_>>()
        );
        assert_eq!(first.len(), holders.len());
    }

    /// Two different objects sharing the same replica set must not put the same
    /// peer first, or one node absorbs every remote read in the cluster.
    #[test]
    fn different_objects_do_not_all_prefer_the_same_peer() {
        let holders: Vec<NodeId> = (0..4).map(|_| NodeId::new()).collect();
        let nodes: Vec<_> = holders
            .iter()
            .enumerate()
            .map(|(index, id)| node(*id, index as u64 + 1, NodeState::Healthy))
            .collect();
        let topology = topology(nodes);
        let reader = NodeId::new();

        let leaders: std::collections::BTreeSet<NodeId> = (0..64)
            .map(|_| {
                let replicas = holders
                    .iter()
                    .map(|id| replica(*id, ReplicaState::Healthy))
                    .collect();
                read_candidates(&placement(ObjectId::new(), replicas), &topology, reader)[0].node_id
            })
            .collect();

        assert!(
            leaders.len() > 1,
            "every object chose the same first peer, which concentrates read load"
        );
    }
}
