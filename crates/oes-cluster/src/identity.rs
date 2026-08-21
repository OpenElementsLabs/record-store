//! Persistent node identity.
//!
//! A storage node's identity must survive restarts, container replacement, and
//! address changes. It is therefore stored in its own small file inside the data
//! directory and is never derived from the hostname, IP address, or container
//! identifier.

use std::{
    io,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use oes_core::{ClusterId, NodeId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::version::{CLUSTER_FORMAT_VERSION, STORAGE_FORMAT_VERSION};

/// Durable layout version of the identity file itself.
const IDENTITY_FILE_VERSION: u32 = 1;

/// Maximum accepted identity-file size. The document is a few hundred bytes.
const MAXIMUM_IDENTITY_BYTES: u64 = 8 * 1024;

/// Raft member identifier assigned by the cluster when a node joins.
///
/// The consensus implementation requires a small dense identifier. It is
/// assigned once and stored next to the opaque [`NodeId`], which remains the
/// identity used by every OES API.
pub type RaftNodeId = u64;

/// The durable identity of one storage node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Layout version of the persisted document.
    pub identity_format_version: u32,
    /// Opaque, globally unique, stable node identifier.
    pub node_id: NodeId,
    /// Cluster this node is bound to, once it has joined one.
    #[serde(default)]
    pub cluster_id: Option<ClusterId>,
    /// Consensus member identifier assigned at join time.
    #[serde(default)]
    pub raft_id: Option<RaftNodeId>,
    /// Durable replica-directory layout version written by this node.
    pub storage_format_version: u32,
    /// Durable cluster-catalog layout version written by this node.
    #[serde(default = "default_cluster_format")]
    pub cluster_format_version: u32,
    /// First time this identity was created.
    pub created_at: DateTime<Utc>,
    /// Time the node last bound itself to a cluster.
    #[serde(default)]
    pub joined_at: Option<DateTime<Utc>>,
}

const fn default_cluster_format() -> u32 {
    CLUSTER_FORMAT_VERSION
}

impl NodeIdentity {
    /// Creates a fresh unbound identity.
    #[must_use]
    pub fn create(now: DateTime<Utc>) -> Self {
        Self {
            identity_format_version: IDENTITY_FILE_VERSION,
            node_id: NodeId::new(),
            cluster_id: None,
            raft_id: None,
            storage_format_version: STORAGE_FORMAT_VERSION,
            cluster_format_version: CLUSTER_FORMAT_VERSION,
            created_at: now,
            joined_at: None,
        }
    }

    /// Returns whether this node has already been bound to a cluster.
    #[must_use]
    pub const fn is_bound(&self) -> bool {
        self.cluster_id.is_some()
    }

    /// Binds the identity to a cluster, refusing a conflicting rebind.
    ///
    /// A node that still holds durable state for one cluster must never silently
    /// join a different cluster: doing so would let stale replica data be
    /// presented as authoritative.
    pub fn bind(
        &mut self,
        cluster_id: ClusterId,
        raft_id: RaftNodeId,
        now: DateTime<Utc>,
    ) -> Result<(), IdentityError> {
        match self.cluster_id {
            Some(existing) if existing != cluster_id => Err(IdentityError::ClusterMismatch {
                stored: existing,
                requested: cluster_id,
            }),
            Some(_) => {
                if self.raft_id.is_some_and(|current| current != raft_id) {
                    return Err(IdentityError::MemberMismatch {
                        stored: self.raft_id.unwrap_or_default(),
                        requested: raft_id,
                    });
                }
                self.raft_id = Some(raft_id);
                Ok(())
            }
            None => {
                self.cluster_id = Some(cluster_id);
                self.raft_id = Some(raft_id);
                self.joined_at = Some(now);
                Ok(())
            }
        }
    }
}

/// Failures while loading, creating, or binding a node identity.
#[derive(Debug, Error)]
pub enum IdentityError {
    /// The identity file could not be read or written.
    #[error("node identity file operation '{operation}' failed: {source}")]
    Io {
        /// Stable operation name without internal path details.
        operation: &'static str,
        /// Underlying I/O failure.
        #[source]
        source: io::Error,
    },
    /// The identity document was malformed.
    #[error("node identity file is malformed: {0}")]
    Malformed(String),
    /// The identity document was written by an incompatible build.
    #[error(
        "node identity file version {found} is not supported by this build (expected {expected})"
    )]
    UnsupportedVersion {
        /// Version found on disk.
        found: u32,
        /// Version this build writes.
        expected: u32,
    },
    /// The node already belongs to a different cluster.
    #[error(
        "this node already belongs to cluster {stored} and cannot join cluster {requested}; \
         reset the node data directory or migrate it deliberately"
    )]
    ClusterMismatch {
        /// Cluster recorded on disk.
        stored: ClusterId,
        /// Cluster the operator asked it to join.
        requested: ClusterId,
    },
    /// The cluster tried to reassign an existing consensus member identifier.
    #[error("this node is already consensus member {stored} and cannot become member {requested}")]
    MemberMismatch {
        /// Member identifier recorded on disk.
        stored: RaftNodeId,
        /// Member identifier offered by the cluster.
        requested: RaftNodeId,
    },
}

/// Durable identity file inside a node's data directory.
#[derive(Debug, Clone)]
pub struct NodeIdentityStore {
    path: PathBuf,
}

impl NodeIdentityStore {
    /// Binds the store to `<data_directory>/node-identity.json`.
    #[must_use]
    pub fn new(data_directory: impl AsRef<Path>) -> Self {
        Self {
            path: data_directory.as_ref().join("node-identity.json"),
        }
    }

    /// Returns the identity file location, for diagnostics only.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the existing identity, or creates and persists a new one.
    ///
    /// A restart therefore reuses the same [`NodeId`] instead of generating a
    /// new one, which is what keeps replica placement metadata meaningful.
    pub fn load_or_create(&self, now: DateTime<Utc>) -> Result<NodeIdentity, IdentityError> {
        if let Some(identity) = self.load()? {
            return Ok(identity);
        }
        let identity = NodeIdentity::create(now);
        self.save(&identity)?;
        Ok(identity)
    }

    /// Loads the identity when the file exists.
    pub fn load(&self) -> Result<Option<NodeIdentity>, IdentityError> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(IdentityError::Io {
                    operation: "inspect node identity",
                    source,
                });
            }
        };
        if metadata.len() > MAXIMUM_IDENTITY_BYTES {
            return Err(IdentityError::Malformed(
                "identity document is implausibly large".into(),
            ));
        }
        let encoded = std::fs::read(&self.path).map_err(|source| IdentityError::Io {
            operation: "read node identity",
            source,
        })?;
        let identity: NodeIdentity = serde_json::from_slice(&encoded)
            .map_err(|error| IdentityError::Malformed(error.to_string()))?;
        if identity.identity_format_version != IDENTITY_FILE_VERSION {
            return Err(IdentityError::UnsupportedVersion {
                found: identity.identity_format_version,
                expected: IDENTITY_FILE_VERSION,
            });
        }
        if identity.storage_format_version > STORAGE_FORMAT_VERSION {
            return Err(IdentityError::Malformed(format!(
                "durable storage format version {} is newer than this build supports ({})",
                identity.storage_format_version, STORAGE_FORMAT_VERSION
            )));
        }
        Ok(Some(identity))
    }

    /// Atomically replaces the identity document.
    pub fn save(&self, identity: &NodeIdentity) -> Result<(), IdentityError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| IdentityError::Malformed("identity path has no parent".into()))?;
        std::fs::create_dir_all(parent).map_err(|source| IdentityError::Io {
            operation: "create identity directory",
            source,
        })?;
        let encoded = serde_json::to_vec_pretty(identity).map_err(|error| IdentityError::Io {
            operation: "encode node identity",
            source: io::Error::other(error),
        })?;
        let temporary = self.path.with_extension("json.tmp");
        write_atomically(&temporary, &self.path, parent, &encoded)
    }

    /// Loads the identity and binds it to a cluster, persisting the result.
    pub fn bind(
        &self,
        cluster_id: ClusterId,
        raft_id: RaftNodeId,
        now: DateTime<Utc>,
    ) -> Result<NodeIdentity, IdentityError> {
        let mut identity = self.load_or_create(now)?;
        identity.bind(cluster_id, raft_id, now)?;
        self.save(&identity)?;
        Ok(identity)
    }
}

fn write_atomically(
    temporary: &Path,
    destination: &Path,
    parent: &Path,
    contents: &[u8],
) -> Result<(), IdentityError> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temporary)
        .map_err(|source| IdentityError::Io {
            operation: "create identity temporary file",
            source,
        })?;
    file.write_all(contents)
        .map_err(|source| IdentityError::Io {
            operation: "write node identity",
            source,
        })?;
    file.sync_all().map_err(|source| IdentityError::Io {
        operation: "synchronize node identity",
        source,
    })?;
    drop(file);
    std::fs::rename(temporary, destination).map_err(|source| IdentityError::Io {
        operation: "publish node identity",
        source,
    })?;
    if let Ok(directory) = std::fs::File::open(parent) {
        // A failure to fsync the directory is not fatal: the rename is already
        // durable on every filesystem OES supports for its data directory.
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_restart() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = NodeIdentityStore::new(directory.path());
        let first = store
            .load_or_create(Utc::now())
            .expect("create node identity");
        let second = store
            .load_or_create(Utc::now())
            .expect("reload node identity");
        assert_eq!(first.node_id, second.node_id);
        assert_eq!(first.created_at, second.created_at);
        assert!(!second.is_bound());
    }

    #[test]
    fn binding_is_idempotent_and_refuses_a_foreign_cluster() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = NodeIdentityStore::new(directory.path());
        let cluster = ClusterId::new();
        let bound = store.bind(cluster, 7, Utc::now()).expect("bind identity");
        assert_eq!(bound.cluster_id, Some(cluster));
        assert_eq!(bound.raft_id, Some(7));
        let rebound = store.bind(cluster, 7, Utc::now()).expect("rebind identity");
        assert_eq!(rebound.node_id, bound.node_id);
        let error = store
            .bind(ClusterId::new(), 7, Utc::now())
            .expect_err("foreign cluster must be refused");
        assert!(matches!(error, IdentityError::ClusterMismatch { .. }));
    }

    #[test]
    fn member_reassignment_is_refused() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = NodeIdentityStore::new(directory.path());
        let cluster = ClusterId::new();
        store.bind(cluster, 1, Utc::now()).expect("bind identity");
        let error = store
            .bind(cluster, 2, Utc::now())
            .expect_err("member reassignment must be refused");
        assert!(matches!(error, IdentityError::MemberMismatch { .. }));
    }

    #[test]
    fn malformed_documents_are_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = NodeIdentityStore::new(directory.path());
        std::fs::write(store.path(), b"{not json").expect("write malformed identity");
        assert!(matches!(store.load(), Err(IdentityError::Malformed(_))));
    }
}
