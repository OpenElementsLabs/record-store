use std::sync::Arc;

use chrono::Utc;
use record_store_core::{Checksum, ClusterId, NodeId, ObjectId};

use crate::command::{ClusterCommand, ClusterIdentity, ClusterOutcome};
use crate::config::ClusterConfig;
use crate::replica::PayloadPlacement;
use crate::tasks::ReplicaTask;
use crate::version::CLUSTER_FORMAT_VERSION;

use super::*;
use crate::{
    replica::Replica,
    tasks::{ReplicaTaskKind, ReplicaTaskPriority},
    topology::{FailureDomain, NodeCapacity, NodeRegistration, StorageClass},
    version::NodeVersions,
};

async fn open_catalog() -> (tempfile::TempDir, ClusterCatalog) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let catalog = ClusterCatalog::open(directory.path().join("cluster.redb"))
        .await
        .expect("open catalog");
    (directory, catalog)
}

fn identity() -> ClusterIdentity {
    ClusterIdentity {
        cluster_id: ClusterId::new(),
        cluster_format_version: CLUSTER_FORMAT_VERSION,
        created_at: Utc::now(),
    }
}

fn registration() -> NodeRegistration {
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

async fn initialized() -> (tempfile::TempDir, ClusterCatalog) {
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

#[tokio::test]
async fn initialization_is_idempotent_and_refuses_a_foreign_cluster() {
    let (_directory, catalog) = open_catalog().await;
    let first = identity();
    catalog
        .apply(ClusterCommand::InitializeCluster {
            identity: first.clone(),
            config: Box::new(ClusterConfig::default()),
        })
        .await
        .expect("initialize");
    catalog
        .apply(ClusterCommand::InitializeCluster {
            identity: first.clone(),
            config: Box::new(ClusterConfig::default()),
        })
        .await
        .expect("re-initialization with the same identity is a no-op");
    let error = catalog
        .apply(ClusterCommand::InitializeCluster {
            identity: identity(),
            config: Box::new(ClusterConfig::default()),
        })
        .await
        .expect_err("a different cluster identity must be refused");
    assert!(matches!(error, ClusterCatalogError::AlreadyInitialized(_)));
}

#[tokio::test]
async fn registration_assigns_stable_member_identifiers() {
    let (_directory, catalog) = initialized().await;
    let first = registration();
    let node_id = first.node_id;
    let outcome = catalog
        .apply(ClusterCommand::RegisterNode {
            registration: Box::new(first.clone()),
            at: Utc::now(),
        })
        .await
        .expect("register");
    let ClusterOutcome::Registration {
        raft_id, created, ..
    } = outcome
    else {
        panic!("registration must return a member identifier");
    };
    assert_eq!(raft_id, 1);
    assert!(created);

    let second = catalog
        .apply(ClusterCommand::RegisterNode {
            registration: Box::new(first),
            at: Utc::now(),
        })
        .await
        .expect("re-register");
    let ClusterOutcome::Registration {
        raft_id: again,
        created: created_again,
        ..
    } = second
    else {
        panic!("re-registration must return a member identifier");
    };
    assert_eq!(again, 1, "restart must not change the member identifier");
    assert!(!created_again);

    let other = catalog
        .apply(ClusterCommand::RegisterNode {
            registration: Box::new(registration()),
            at: Utc::now(),
        })
        .await
        .expect("register second node");
    let ClusterOutcome::Registration { raft_id, .. } = other else {
        panic!("registration must return a member identifier");
    };
    assert_eq!(raft_id, 2);
    assert!(catalog.node(node_id).await.expect("read").is_some());
    assert!(
        catalog
            .node_by_member(1)
            .await
            .expect("read")
            .is_some_and(|node| node.node_id == node_id)
    );
}

#[tokio::test]
async fn only_the_configured_number_of_nodes_become_voters() {
    let (_directory, catalog) = initialized().await;
    let mut voters = 0;
    for _ in 0..5 {
        let outcome = catalog
            .apply(ClusterCommand::RegisterNode {
                registration: Box::new(registration()),
                at: Utc::now(),
            })
            .await
            .expect("register");
        if outcome.node().is_some_and(|node| node.metadata_voter) {
            voters += 1;
        }
    }
    assert_eq!(voters, 3, "voter count must follow metadata_voter_target");
}

#[tokio::test]
async fn deleting_a_placement_creates_a_tombstone_for_every_holder() {
    let (_directory, catalog) = initialized().await;
    let first = NodeId::new();
    let second = NodeId::new();
    let object_id = ObjectId::new();
    let now = Utc::now();
    let placement = PayloadPlacement::new(
        object_id,
        100,
        Checksum::sha256([1_u8; 32]),
        2,
        StorageClass::default(),
        vec![
            Replica::healthy(first, 100, Checksum::sha256([1_u8; 32]), now),
            Replica::healthy(second, 100, Checksum::sha256([1_u8; 32]), now),
        ],
        now,
    );
    catalog
        .apply(ClusterCommand::PutPlacement {
            placement: Box::new(placement),
        })
        .await
        .expect("commit placement");
    let usage = catalog.usage().await.expect("usage");
    assert_eq!(usage.payloads, 1);
    assert_eq!(usage.logical_bytes, 100);
    assert_eq!(usage.physical_bytes, 200);

    catalog
        .apply(ClusterCommand::DeletePlacement { object_id, at: now })
        .await
        .expect("delete placement");
    let tombstone = catalog
        .tombstone(object_id)
        .await
        .expect("read tombstone")
        .expect("tombstone must exist");
    assert_eq!(tombstone.pending_nodes.len(), 2);
    assert!(!tombstone.completed());
    let usage = catalog.usage().await.expect("usage");
    assert_eq!(usage.payloads, 0);
    assert_eq!(usage.physical_bytes, 0);
    assert_eq!(usage.tombstones, 1);

    catalog
        .apply(ClusterCommand::AcknowledgeTombstone {
            object_id,
            node_id: first,
            at: now,
        })
        .await
        .expect("acknowledge");
    catalog
        .apply(ClusterCommand::AcknowledgeTombstone {
            object_id,
            node_id: second,
            at: now,
        })
        .await
        .expect("acknowledge");
    let tombstone = catalog
        .tombstone(object_id)
        .await
        .expect("read tombstone")
        .expect("tombstone must exist");
    assert!(tombstone.completed());
}

#[tokio::test]
async fn identical_task_requests_do_not_duplicate_work() {
    let (_directory, catalog) = initialized().await;
    let object_id = ObjectId::new();
    let now = Utc::now();
    let first = catalog
        .apply(ClusterCommand::EnqueueTask {
            task: Box::new(ReplicaTask::queued(
                object_id,
                ReplicaTaskKind::Repair,
                ReplicaTaskPriority::High,
                10,
                now,
            )),
        })
        .await
        .expect("enqueue")
        .task()
        .expect("task");
    let second = catalog
        .apply(ClusterCommand::EnqueueTask {
            task: Box::new(ReplicaTask::queued(
                object_id,
                ReplicaTaskKind::Repair,
                ReplicaTaskPriority::High,
                10,
                now,
            )),
        })
        .await
        .expect("enqueue")
        .task()
        .expect("task");
    assert_eq!(
        first.id, second.id,
        "duplicate repair requests must collapse"
    );
    let usage = catalog.usage().await.expect("usage");
    assert_eq!(usage.active_tasks, 1);
}

#[tokio::test]
async fn queued_tasks_are_returned_in_risk_order() {
    let (_directory, catalog) = initialized().await;
    let now = Utc::now();
    for (kind, priority) in [
        (ReplicaTaskKind::Rebalance, ReplicaTaskPriority::Low),
        (ReplicaTaskKind::Repair, ReplicaTaskPriority::Unavailable),
        (ReplicaTaskKind::Drain, ReplicaTaskPriority::Normal),
    ] {
        catalog
            .apply(ClusterCommand::EnqueueTask {
                task: Box::new(ReplicaTask::queued(
                    ObjectId::new(),
                    kind,
                    priority,
                    10,
                    now,
                )),
            })
            .await
            .expect("enqueue");
    }
    let page = catalog.queued_tasks(10).await.expect("queued tasks");
    let priorities: Vec<_> = page.tasks.iter().map(|task| task.priority).collect();
    assert_eq!(
        priorities,
        vec![
            ReplicaTaskPriority::Unavailable,
            ReplicaTaskPriority::Normal,
            ReplicaTaskPriority::Low
        ]
    );
}

#[tokio::test]
async fn snapshot_export_and_import_round_trip() {
    let (_directory, catalog) = initialized().await;
    catalog
        .apply(ClusterCommand::RegisterNode {
            registration: Box::new(registration()),
            at: Utc::now(),
        })
        .await
        .expect("register");
    let database = catalog.database();
    let entries = tokio::task::spawn_blocking({
        let database = Arc::clone(&database);
        move || {
            let read = database.begin_read().expect("begin");
            export_tx(&read).expect("export")
        }
    })
    .await
    .expect("join");
    assert!(!entries.is_empty());

    let (_other_directory, other) = open_catalog().await;
    let other_database = other.database();
    tokio::task::spawn_blocking(move || {
        let write = other_database.begin_write().expect("begin");
        import_tx(&write, &entries).expect("import");
        write.commit().expect("commit");
    })
    .await
    .expect("join");
    let restored = other.nodes().await.expect("nodes");
    assert_eq!(restored.len(), 1);
    assert!(other.identity().await.expect("identity").is_some());
}
