//! Operator-driven recovery of metadata authority.
//!
//! Everything else in this crate is what the cluster does for itself. This
//! module is the opposite: it is the one procedure that only a human may start,
//! because the decision it encodes — "a majority of the metadata voters is
//! never coming back" — cannot be made from inside a partition. A cluster that
//! concluded that on its own would be a cluster that promotes a minority every
//! time a switch reboots.
//!
//! What recovery does is narrow on purpose. It rebuilds the *membership* of the
//! consensus group around one surviving member and leaves the replicated state
//! machine exactly as it was: object history, versions, retention and Object
//! Lock, credentials, placement, and the cluster's own identity all survive
//! byte-for-byte. It never merges divergent state, never picks a winner between
//! two copies, and never invents an entry that was not already committed on the
//! member it runs against.
//!
//! What it costs is stated rather than implied. Any entry that was committed by
//! the old quorum but had not reached this member is gone, and the report says
//! how much of the data plane that affects. Recovery runs offline, against a
//! stopped node, so there is no window in which the old group and the new one
//! are both live.
//!
//! The one mistake that genuinely cannot be undone is running this on two
//! surviving members separately: that produces two clusters holding one
//! identifier. It cannot be prevented from inside a single node, so instead
//! every recovery stamps a distinct lineage into the cluster identity, which
//! makes the split detectable the moment the two meet.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use openraft::{LogId, Membership};
use record_store_cluster::{ClusterCatalog, ClusterCommand, ClusterIdentity, NodeState};
use record_store_core::{ClusterId, NodeId, open_database};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    log_store::{COMMITTED, ENTRIES, LAST_PURGED, STATE},
    state_machine::{read_applied_state, record_membership},
    types::{ConsensusMembership, MemberId, MemberNode},
};

/// File names inside a member's consensus directory.
const LOG_FILE: &str = "consensus-log.redb";
const STATE_FILE: &str = "consensus-state.redb";
const SNAPSHOT_DIRECTORY: &str = "snapshots";
const SNAPSHOT_POINTER: &str = "current.json";

/// Failures raised by inspection and recovery.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// The consensus directory does not hold a member's durable state.
    #[error("{0} does not contain consensus state; there is nothing to recover from here")]
    NoConsensusState(PathBuf),
    /// Durable state could not be read or written.
    #[error("consensus state could not be {operation}: {reason}")]
    Storage {
        /// What was being attempted.
        operation: &'static str,
        /// Why it failed.
        reason: String,
    },
    /// A blocking recovery task could not finish.
    #[error("recovery task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// This member has never applied anything, so it holds no authority to rebuild.
    #[error(
        "this member has applied no metadata, so it holds nothing to recover; recovering from it \
         would create an empty cluster wearing the old cluster's name"
    )]
    NothingApplied,
    /// The member's state does not belong to the cluster the operator named.
    #[error(
        "this member holds metadata for cluster {stored}, not {requested}; refusing to recover a \
         cluster this node was never part of"
    )]
    ClusterMismatch {
        /// Cluster the durable state belongs to.
        stored: ClusterId,
        /// Cluster the operator named.
        requested: ClusterId,
    },
    /// The state holds no cluster identity at all.
    #[error("this member holds no cluster identity; it was never a member of a formed cluster")]
    NotInitialized,
    /// The operator named a member that is not in the recorded membership.
    #[error(
        "member {requested} is not part of the recorded consensus membership ({members}); \
         recovery must run against a member the cluster actually had"
    )]
    NotAMember {
        /// Member the operator named.
        requested: MemberId,
        /// Members the durable state records.
        members: String,
    },
    /// The operator did not acknowledge what recovery costs.
    #[error(
        "recovery discards any metadata the lost quorum committed but never replicated here, and \
         cannot be undone; it requires an explicit acknowledgement of that loss"
    )]
    LossNotAcknowledged,
    /// The cluster catalog could not be read.
    #[error("cluster catalog could not be read: {0}")]
    Catalog(#[from] record_store_cluster::ClusterCatalogError),
}

fn storage<E: std::fmt::Display>(operation: &'static str) -> impl Fn(E) -> RecoveryError {
    move |error| RecoveryError::Storage {
        operation,
        reason: error.to_string(),
    }
}

/// What a member's on-disk snapshot looks like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SnapshotHealth {
    /// No snapshot has been published.
    Absent,
    /// A snapshot is present and its body parses.
    Present {
        /// Log position the snapshot covers.
        index: Option<u64>,
    },
    /// A snapshot is referenced but is unusable.
    ///
    /// Reported rather than hidden. The state machine, not the snapshot, is what
    /// a member restarts from, so a damaged snapshot does not stop the node —
    /// but an operator diagnosing a failed transfer needs to be told.
    Damaged {
        /// What is wrong with it.
        reason: String,
    },
}

/// A read-only assessment of one member's durable consensus state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryAssessment {
    /// The cluster this member's state belongs to, if any.
    pub cluster: Option<ClusterIdentity>,
    /// Log position this member has applied.
    pub last_applied: Option<u64>,
    /// Voters recorded in this member's last known membership.
    pub voters: Vec<MemberId>,
    /// Every member recorded, voter or learner, with its address.
    pub members: BTreeMap<MemberId, String>,
    /// Unapplied log entries still on disk.
    pub log_entries: u64,
    /// Health of the published snapshot.
    pub snapshot: SnapshotHealth,
    /// Whether this state could be recovered from at all.
    pub recoverable: bool,
    /// Why, in an operator's words.
    pub summary: String,
}

/// What an operator is asking for, stated explicitly.
///
/// Every field exists to make a mistyped or half-considered recovery fail
/// rather than succeed. Naming the cluster catches the wrong data directory;
/// naming the member catches recovering as a node that was never in the group;
/// the acknowledgement catches someone who has not understood that committed
/// metadata may be discarded.
#[derive(Debug, Clone)]
pub struct RecoveryIntent {
    /// Cluster the operator believes this member belongs to.
    pub cluster_id: ClusterId,
    /// Member to rebuild authority around.
    pub member_id: MemberId,
    /// Address that member will advertise.
    pub address: String,
    /// Why this is being done. Kept in the cluster's own record.
    pub reason: String,
    /// Explicit acknowledgement that unreplicated commits are lost.
    pub accept_data_loss: bool,
}

/// What a completed recovery did, and what it cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryReport {
    /// Cluster whose authority was rebuilt.
    pub cluster_id: ClusterId,
    /// Identifier of this specific recovery.
    pub recovery_id: Uuid,
    /// How many times this cluster's authority has now been rebuilt.
    pub recovery_generation: u32,
    /// Member that now holds authority alone.
    pub member_id: MemberId,
    /// Log position the recovered member carries forward.
    pub last_applied: Option<u64>,
    /// Voters that were removed from the group.
    pub removed_voters: Vec<MemberId>,
    /// Unapplied log entries discarded, which is the metadata that was lost.
    pub discarded_log_entries: u64,
    /// Snapshot files removed because they carried the old membership.
    pub discarded_snapshots: u64,
    /// Payloads the cluster knows about.
    pub payloads_total: u64,
    /// Payloads with at least one replica recorded on this member.
    pub payloads_held_here: u64,
    /// Payloads with no replica on this member.
    ///
    /// Readable only once one of their other holders returns. This is the
    /// availability deficit recovery leaves behind, and it is reported rather
    /// than left for an operator to discover through a failed read.
    pub payloads_elsewhere: u64,
    /// Data-plane nodes still recorded, other than this one.
    pub other_nodes: Vec<NodeId>,
    /// Replicas the cluster's policy wants for each payload.
    pub replication_factor: u8,
    /// Replicas a write must reach before it is acknowledged.
    ///
    /// Reported because a recovered cluster is frequently readable and not
    /// writable: one surviving member cannot satisfy an acknowledgement
    /// requirement of two, and the cluster will refuse writes rather than
    /// quietly weakening the policy. An operator needs that stated here, not
    /// discovered through a failed upload.
    pub required_acknowledgements: u8,
}

impl RecoveryReport {
    /// Returns whether the recovered cluster can currently read everything.
    #[must_use]
    pub const fn fully_readable(&self) -> bool {
        self.payloads_elsewhere == 0
    }

    /// Returns whether one member alone can satisfy the write policy.
    ///
    /// When this is false the cluster comes back readable but not writable, and
    /// it will refuse writes rather than acknowledge them below the policy. That
    /// is the intended behaviour; restoring capacity, or deliberately lowering
    /// the policy, is an operator decision.
    #[must_use]
    pub const fn writable_alone(&self) -> bool {
        self.required_acknowledgements <= 1
    }
}

/// Returns whether a directory holds a member's durable consensus state.
///
/// Startup uses this before it creates or binds anything. A node whose identity
/// file is missing while this is true has lost the identity that owned its
/// data — and minting a replacement, however briefly, would be a durable change
/// made on the way to refusing.
#[must_use]
pub fn holds_consensus_state(directory: impl AsRef<Path>) -> bool {
    state_path(directory.as_ref()).exists()
}

fn state_path(directory: &Path) -> PathBuf {
    directory.join(STATE_FILE)
}

fn log_path(directory: &Path) -> PathBuf {
    directory.join(LOG_FILE)
}

fn open(path: &Path, operation: &'static str) -> Result<Arc<Database>, RecoveryError> {
    open_database(path)
        .map(Arc::new)
        .map_err(storage(operation))
}

/// Reports what a member's durable consensus state holds, changing nothing.
///
/// This is the step an operator runs first, and the one they can run on every
/// survivor before choosing which to recover from. Choosing badly is the real
/// risk in a disaster, and it cannot be chosen well without seeing each
/// member's applied position side by side.
pub async fn inspect(directory: impl AsRef<Path>) -> Result<RecoveryAssessment, RecoveryError> {
    let directory = directory.as_ref().to_path_buf();
    if !state_path(&directory).exists() {
        return Err(RecoveryError::NoConsensusState(directory));
    }
    assess(&directory).await
}

async fn assess(directory: &Path) -> Result<RecoveryAssessment, RecoveryError> {
    let state = open(&state_path(directory), "opened")?;
    let (last_applied, membership) = applied_state(&state).await?;
    let catalog = ClusterCatalog::from_database(Arc::clone(&state))?;
    let cluster = catalog.identity().await?;
    let log_entries = count_log_entries(directory).await?;
    let snapshot = snapshot_health(&directory.join(SNAPSHOT_DIRECTORY));
    let voters: Vec<MemberId> = membership.membership().voter_ids().collect();
    let members: BTreeMap<MemberId, String> = membership
        .membership()
        .nodes()
        .map(|(id, node)| (*id, node.addr.clone()))
        .collect();

    let recoverable = last_applied.is_some() && cluster.is_some();
    let summary = match (&cluster, last_applied) {
        (Some(identity), Some(log_id)) => format!(
            "member of cluster {} at log index {}, with {} recorded voter(s) and {} unapplied \
             log entr{}",
            identity.cluster_id,
            log_id.index,
            voters.len(),
            log_entries,
            if log_entries == 1 { "y" } else { "ies" }
        ),
        (Some(identity), None) => format!(
            "bound to cluster {} but has applied nothing; there is no authority here to rebuild",
            identity.cluster_id
        ),
        (None, _) => {
            "holds no cluster identity; this member was never part of a formed cluster".to_owned()
        }
    };

    Ok(RecoveryAssessment {
        cluster,
        last_applied: last_applied.map(|log_id| log_id.index),
        voters,
        members,
        log_entries,
        snapshot,
        recoverable,
        summary,
    })
}

/// Reads the applied position and membership off the state database.
async fn applied_state(
    state: &Arc<Database>,
) -> Result<(Option<LogId<MemberId>>, ConsensusMembership), RecoveryError> {
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        let read = state.begin_read().map_err(storage("read"))?;
        read_applied_state(&read).map_err(storage("read the applied position"))
    })
    .await?
}

/// Counts log entries still on disk, which is the metadata recovery discards.
async fn count_log_entries(directory: &Path) -> Result<u64, RecoveryError> {
    let path = log_path(directory);
    if !path.exists() {
        return Ok(0);
    }
    let log = open(&path, "opened")?;
    tokio::task::spawn_blocking(move || {
        let read = log.begin_read().map_err(storage("read"))?;
        let table = read.open_table(ENTRIES).map_err(storage("read the log"))?;
        table.len().map_err(storage("count the log"))
    })
    .await?
}

fn snapshot_health(directory: &Path) -> SnapshotHealth {
    let pointer_path = directory.join(SNAPSHOT_POINTER);
    let encoded = match std::fs::read(&pointer_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SnapshotHealth::Absent;
        }
        Err(error) => {
            return SnapshotHealth::Damaged {
                reason: format!("the snapshot pointer could not be read: {error}"),
            };
        }
    };
    let pointer: serde_json::Value = match serde_json::from_slice(&encoded) {
        Ok(value) => value,
        Err(error) => {
            return SnapshotHealth::Damaged {
                reason: format!("the snapshot pointer is not valid JSON: {error}"),
            };
        }
    };
    let Some(id) = pointer
        .get("snapshot_id")
        .and_then(serde_json::Value::as_str)
    else {
        return SnapshotHealth::Damaged {
            reason: "the snapshot pointer names no snapshot".to_owned(),
        };
    };
    let body = directory.join(format!("{id}.snapshot"));
    match std::fs::read(&body) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(_) => SnapshotHealth::Present {
                index: pointer
                    .get("last_applied")
                    .and_then(|value| value.get("index"))
                    .and_then(serde_json::Value::as_u64),
            },
            Err(error) => SnapshotHealth::Damaged {
                reason: format!("snapshot '{id}' is present but does not parse: {error}"),
            },
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SnapshotHealth::Damaged {
            reason: format!(
                "the snapshot pointer names '{id}', which is not on disk; a snapshot transfer or \
                 publication was interrupted"
            ),
        },
        Err(error) => SnapshotHealth::Damaged {
            reason: format!("snapshot '{id}' could not be read: {error}"),
        },
    }
}

/// Rebuilds metadata authority around one surviving member.
///
/// The member must be stopped. Nothing here contacts a peer, and nothing waits
/// for one: that is the point, because the peers are what is gone.
pub async fn recover_single_member(
    directory: impl AsRef<Path>,
    intent: RecoveryIntent,
) -> Result<RecoveryReport, RecoveryError> {
    let directory = directory.as_ref().to_path_buf();
    if !intent.accept_data_loss {
        return Err(RecoveryError::LossNotAcknowledged);
    }
    if !state_path(&directory).exists() {
        return Err(RecoveryError::NoConsensusState(directory));
    }
    rebuild(&directory, &intent).await
}

async fn rebuild(
    directory: &Path,
    intent: &RecoveryIntent,
) -> Result<RecoveryReport, RecoveryError> {
    let state = open(&state_path(directory), "opened")?;
    let (last_applied, membership) = applied_state(&state).await?;
    let last_applied = last_applied.ok_or(RecoveryError::NothingApplied)?;
    let catalog = ClusterCatalog::from_database(Arc::clone(&state))?;
    let identity = catalog
        .identity()
        .await?
        .ok_or(RecoveryError::NotInitialized)?;
    if identity.cluster_id != intent.cluster_id {
        return Err(RecoveryError::ClusterMismatch {
            stored: identity.cluster_id,
            requested: intent.cluster_id,
        });
    }

    let recorded: BTreeSet<MemberId> = membership.membership().nodes().map(|(id, _)| *id).collect();
    // Recovering as a member the group never had would fabricate authority
    // rather than rebuild it, which is the one thing this procedure must not do.
    if !recorded.contains(&intent.member_id) {
        return Err(RecoveryError::NotAMember {
            requested: intent.member_id,
            members: recorded
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    let removed_voters: Vec<MemberId> = membership
        .membership()
        .voter_ids()
        .filter(|id| *id != intent.member_id)
        .collect();

    let now = Utc::now();
    let recovery_id = Uuid::new_v4();
    let rebuilt = single_member_membership(intent, last_applied);

    // The membership change and the cluster's record of the recovery commit in
    // one transaction. Half of this would be worse than neither: a group that
    // elected itself without recording why, or a record of a recovery that did
    // not happen.
    {
        let database = Arc::clone(&state);
        let reason = intent.reason.clone();
        let rebuilt = rebuilt.clone();
        tokio::task::spawn_blocking(move || -> Result<(), RecoveryError> {
            let write = database.begin_write().map_err(storage("write"))?;
            record_membership(&write, &rebuilt).map_err(storage("rewrite the membership"))?;
            record_store_cluster::apply_command_tx(
                &write,
                ClusterCommand::RecordRecovery {
                    recovery_id,
                    reason,
                    at: now,
                },
            )?;
            write.commit().map_err(storage("commit the recovery"))?;
            Ok(())
        })
        .await??;
    }

    let discarded_log_entries = reset_log(directory, last_applied).await?;
    // A snapshot built before the recovery carries the old membership. Shipping
    // one to a node that rejoins later would quietly re-add every voter this
    // procedure just removed, so the stale snapshots go with the old log.
    let discarded_snapshots = discard_snapshots(&directory.join(SNAPSHOT_DIRECTORY))?;

    let recovered = catalog
        .identity()
        .await?
        .ok_or(RecoveryError::NotInitialized)?;
    let (payloads_total, payloads_held_here, other_nodes) =
        survey(&catalog, intent.member_id).await?;
    let config = catalog.config().await?.unwrap_or_default();

    info!(
        cluster = %identity.cluster_id,
        member = intent.member_id,
        generation = recovered.recovery_generation,
        discarded_log_entries,
        "rebuilt metadata authority around a single surviving member"
    );

    Ok(RecoveryReport {
        cluster_id: identity.cluster_id,
        recovery_id,
        recovery_generation: recovered.recovery_generation,
        member_id: intent.member_id,
        last_applied: Some(last_applied.index),
        removed_voters,
        discarded_log_entries,
        discarded_snapshots,
        payloads_total,
        payloads_held_here,
        payloads_elsewhere: payloads_total.saturating_sub(payloads_held_here),
        other_nodes,
        replication_factor: config.replication_factor,
        required_acknowledgements: config.required_acknowledgements(),
    })
}

fn single_member_membership(intent: &RecoveryIntent, at: LogId<MemberId>) -> ConsensusMembership {
    let voters: BTreeSet<MemberId> = [intent.member_id].into_iter().collect();
    let nodes: BTreeMap<MemberId, MemberNode> = [(
        intent.member_id,
        MemberNode {
            addr: intent.address.clone(),
        },
    )]
    .into_iter()
    .collect();
    ConsensusMembership::new(Some(at), Membership::new(vec![voters], nodes))
}

/// Discards the log and anchors it at the applied position.
///
/// Every remaining entry is either already applied — in which case replaying it
/// would be pointless — or was never committed here, in which case keeping it
/// would let the recovered member resurrect a decision the old quorum may never
/// have made. Returning the count is how the report can state the loss.
async fn reset_log(directory: &Path, last_applied: LogId<MemberId>) -> Result<u64, RecoveryError> {
    let path = log_path(directory);
    if !path.exists() {
        return Ok(0);
    }
    let log = open(&path, "opened")?;
    tokio::task::spawn_blocking(move || reset_log_blocking(&log, last_applied)).await?
}

fn reset_log_blocking(
    log: &Arc<Database>,
    last_applied: LogId<MemberId>,
) -> Result<u64, RecoveryError> {
    let write = log.begin_write().map_err(storage("write the log"))?;
    let discarded;
    {
        let mut entries = write.open_table(ENTRIES).map_err(storage("open the log"))?;
        discarded = entries
            .iter()
            .map_err(storage("scan the log"))?
            .filter_map(Result::ok)
            .filter(|(index, _)| index.value() > last_applied.index)
            .count() as u64;
        entries
            .retain(|_, _| false)
            .map_err(storage("clear the log"))?;
    }
    {
        let mut state = write.open_table(STATE).map_err(storage("open log state"))?;
        let purged = serde_json::to_vec(&Some(last_applied)).map_err(storage("encode"))?;
        state
            .insert(LAST_PURGED, purged.as_slice())
            .map_err(storage("anchor the log"))?;
        let committed = serde_json::to_vec(&Some(last_applied)).map_err(storage("encode"))?;
        state
            .insert(COMMITTED, committed.as_slice())
            .map_err(storage("anchor the commit index"))?;
    }
    write.commit().map_err(storage("commit the log reset"))?;
    Ok(discarded)
}

fn discard_snapshots(directory: &Path) -> Result<u64, RecoveryError> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_snapshot = path
            .extension()
            .is_some_and(|extension| extension == "snapshot")
            || path
                .file_name()
                .is_some_and(|name| name == SNAPSHOT_POINTER);
        if is_snapshot {
            if let Err(error) = std::fs::remove_file(&path) {
                warn!(path = %path.display(), %error, "a stale snapshot could not be removed");
            } else {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Counts what the recovered cluster can and cannot read.
async fn survey(
    catalog: &ClusterCatalog,
    member_id: MemberId,
) -> Result<(u64, u64, Vec<NodeId>), RecoveryError> {
    let nodes = catalog.nodes().await?;
    let this_node = nodes
        .iter()
        .find(|node| node.raft_id == member_id)
        .map(|node| node.node_id);
    let other_nodes: Vec<NodeId> = nodes
        .iter()
        .filter(|node| Some(node.node_id) != this_node && node.state != NodeState::Decommissioned)
        .map(|node| node.node_id)
        .collect();

    let mut cursor = None;
    let mut total = 0_u64;
    let mut here = 0_u64;
    loop {
        let page = catalog.list_placements(cursor, 512).await?;
        if page.placements.is_empty() {
            break;
        }
        for placement in &page.placements {
            total += 1;
            if this_node.is_some_and(|node_id| placement.replica(node_id).is_some()) {
                here += 1;
            }
        }
        cursor = page.next_object_id;
        if cursor.is_none() {
            break;
        }
    }
    Ok((total, here, other_nodes))
}
