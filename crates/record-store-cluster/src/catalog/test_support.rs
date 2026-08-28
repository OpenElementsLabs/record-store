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
