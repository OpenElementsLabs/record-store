//! Shared fixtures for cluster catalog tests.

use chrono::Utc;
use record_store_core::{ClusterId, NodeId};

use crate::catalog::ClusterCatalog;
use crate::command::{ClusterCommand, ClusterIdentity};
use crate::config::ClusterConfig;
use crate::topology::{FailureDomain, NodeCapacity, NodeRegistration, StorageClass};
use crate::version::{CLUSTER_FORMAT_VERSION, NodeVersions};

pub(crate) async fn open_catalog() -> (tempfile::TempDir, ClusterCatalog) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let catalog = ClusterCatalog::open(directory.path().join("cluster.redb"))
        .await
        .expect("open catalog");
    (directory, catalog)
}

pub(crate) fn identity() -> ClusterIdentity {
    ClusterIdentity {
        cluster_id: ClusterId::new(),
        cluster_format_version: CLUSTER_FORMAT_VERSION,
        created_at: Utc::now(),
    }
}

pub(crate) fn registration() -> NodeRegistration {
    NodeRegistration {
        node_id: NodeId::new(),
        versions: NodeVersions::current("test"),
        rpc_address: "10.0.0.1:7603".into(),
        s3_endpoint: Some("http://10.0.0.1:7600".into()),
        storage_class: StorageClass::default(),
        failure_domain: FailureDomain::parse("rack=a").expect("labels"),
        capacity: NodeCapacity {
            total_bytes: 1_000,
            available_bytes: 900,
            replica_bytes: 100,
            temporary_bytes: 0,
        },
        devices: Vec::new(),
        started_at: Utc::now(),
    }
}

pub(crate) async fn initialized() -> (tempfile::TempDir, ClusterCatalog) {
    let (directory, catalog) = open_catalog().await;
    catalog
        .apply(ClusterCommand::InitializeCluster {
            identity: identity(),
            config: Box::new(ClusterConfig::default()),
        })
        .await
        .expect("initialize cluster");
    (directory, catalog)
}

/// Registers a node and returns its identifier.
pub(crate) async fn register(catalog: &ClusterCatalog, now: chrono::DateTime<Utc>) -> NodeId {
    let registration = registration();
    let node_id = registration.node_id;
    catalog
        .apply(ClusterCommand::RegisterNode {
            registration: Box::new(registration),
            at: now,
        })
        .await
        .expect("register node");
    node_id
}

/// Builds a join token record with the supplied bounds.
pub(crate) fn join_token(
    now: chrono::DateTime<Utc>,
    maximum_uses: u32,
) -> crate::credentials::JoinToken {
    crate::credentials::JoinToken {
        id: record_store_core::JoinTokenId::new(),
        token_digest: [7_u8; 32],
        created_at: now,
        expires_at: now + chrono::Duration::hours(1),
        maximum_uses,
        uses: 0,
        revoked: false,
        description: "test token".to_owned(),
    }
}

/// Builds a long-running operation record.
pub(crate) fn operation(
    kind: crate::tasks::ClusterOperationKind,
    node_id: NodeId,
    now: chrono::DateTime<Utc>,
) -> crate::tasks::ClusterOperation {
    crate::tasks::ClusterOperation {
        id: record_store_core::ClusterOperationId::new(),
        kind,
        node_id: Some(node_id),
        state: crate::tasks::ClusterOperationState::Planning,
        progress: crate::tasks::OperationProgress::default(),
        started_at: now,
        updated_at: now,
        completed_at: None,
        message: None,
    }
}
