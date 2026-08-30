//! Shared fixtures for management API tests.
//!
//! The router is built over real backends in a throwaway directory so the tests
//! exercise the same code paths a deployment does, including authorization and
//! durable side effects.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use record_store_audit::{AuditRepository, RedbAuditRepository};
use record_store_auth::CredentialManager;
use record_store_core::OrganizationId;
use record_store_events::{EventRepository, RedbEventRepository};
use record_store_metadata::{MetadataRepository, RedbMetadataRepository};
use record_store_service::{ServiceLimits, Services};
use record_store_storage::{DeviceStore, LocalFilesystemStore, ObjectStore};
use tempfile::TempDir;
use tower::ServiceExt;

use record_store_sharing::{CapabilityStore, SharingPolicy, SharingService, TicketIssuer};

use crate::sharing::SharingManagement;
use crate::{AppState, ManagementAuth, MetricsAuth, router};

/// The system-administrator token every authenticated fixture request presents.
pub(crate) const SYSTEM_TOKEN: &str = "system-token-at-least-thirty-two-bytes-long";
/// A storage-administrator token, for tests that check role separation.
pub(crate) const STORAGE_TOKEN: &str = "storage-token-at-least-thirty-two-bytes-long";
/// An auditor token, which may only read.
pub(crate) const AUDITOR_TOKEN: &str = "auditor-token-at-least-thirty-two-bytes-long";
/// The Prometheus scrape token the fixture configures.
pub(crate) const METRICS_TOKEN: &str = "metrics-token-at-least-thirty-two-bytes-long";
/// Master key for the capability store the fixture wires in.
const SHARING_KEY: &[u8] = b"sharing-master-key-at-least-32-bytes-long";

/// Builds the management router over real catalog, storage, and audit backends.
pub(crate) async fn api() -> (TempDir, Router) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let metadata: Arc<dyn MetadataRepository> = Arc::new(
        RedbMetadataRepository::open(directory.path().join("metadata.redb"))
            .await
            .expect("metadata repository"),
    );
    let storage: Arc<dyn ObjectStore> = Arc::new(
        LocalFilesystemStore::open(
            directory.path(),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let audit: Arc<dyn AuditRepository> = Arc::new(
        RedbAuditRepository::open(directory.path().join("audit.redb"))
            .await
            .expect("audit repository"),
    );
    let events: Arc<dyn EventRepository> = Arc::new(
        RedbEventRepository::open(
            directory.path().join("events.redb"),
            Some(b"event-master-key-at-least-32-bytes-long"),
            // Loopback delivery is permitted so tests can register a target
            // that actually resolves; the public-network policy is exercised
            // separately by `api_with_public_webhooks_only`.
            record_store_events::WebhookConfig {
                allow_http: true,
                allow_private_networks: true,
                ..record_store_events::WebhookConfig::default()
            },
        )
        .await
        .expect("event repository"),
    );
    let credentials = Arc::new(
        CredentialManager::open(
            directory.path().join("credentials.redb"),
            "root-access-key",
            "root-secret-at-least-sixteen",
            Some(b"credential-master-key-at-least-32-bytes"),
        )
        .await
        .expect("credential manager"),
    );
    let owner = OrganizationId::new();
    let services = Services::new(
        Arc::clone(&storage),
        Arc::clone(&metadata),
        owner,
        ServiceLimits {
            maximum_concurrent_operations: 8,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
        },
    );
    let state = AppState::new(
        storage,
        metadata,
        services,
        credentials,
        audit,
        owner,
        "0.0.0-test",
    )
    .with_events(events)
    .with_sharing(SharingManagement::new(
        Arc::new(SharingService::new(
            CapabilityStore::open(directory.path().join("sharing.redb"), SHARING_KEY)
                .await
                .expect("capability store"),
            SharingPolicy::default(),
            TicketIssuer::derive(SHARING_KEY).expect("ticket issuer"),
        )),
        Some("https://share.example".to_owned()),
        "https://embed.example".to_owned(),
        64 * 1024,
    ))
    .with_management_auth(ManagementAuth::bearer_tokens(
        SYSTEM_TOKEN.as_bytes(),
        Some(STORAGE_TOKEN.as_bytes()),
        Some(AUDITOR_TOKEN.as_bytes()),
    ))
    .with_metrics_auth(MetricsAuth::bearer_token(METRICS_TOKEN.as_bytes()));
    (directory, router(state))
}

/// Sends a request as the system administrator.
pub(crate) async fn call(router: &Router, request: Request<Body>) -> Response<Body> {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds")
}

/// Builds an authenticated request carrying an optional JSON body.
pub(crate) fn signed(
    method: &str,
    uri: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    }
}

/// Builds a request as the system administrator.
pub(crate) fn admin(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
    signed(method, uri, SYSTEM_TOKEN, body)
}

/// Reads a response body as JSON.
pub(crate) async fn json_body(response: Response<Body>) -> serde_json::Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Sends an authenticated request and asserts the resulting status.
pub(crate) async fn expect_status(
    router: &Router,
    request: Request<Body>,
    expected: StatusCode,
) -> serde_json::Value {
    let response = call(router, request).await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, expected, "unexpected status; body was {body}");
    body
}

/// Creates a bucket through the API.
pub(crate) async fn make_bucket(router: &Router, name: &str) {
    let response = call(
        router,
        admin(
            "POST",
            "/api/v1/buckets",
            Some(serde_json::json!({"name": name})),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED, "create {name}");
}

/// Uploads an object through the API and returns the decoded summary.
pub(crate) async fn put_object(
    router: &Router,
    bucket: &str,
    key: &str,
    body: &[u8],
) -> serde_json::Value {
    put_typed_object(router, bucket, key, body, "application/octet-stream").await
}

/// Uploads an object with an explicit media type.
pub(crate) async fn put_typed_object(
    router: &Router,
    bucket: &str,
    key: &str,
    body: &[u8],
    content_type: &str,
) -> serde_json::Value {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/api/v1/buckets/{bucket}/object/{key}"))
        .header("authorization", format!("Bearer {SYSTEM_TOKEN}"))
        .header("content-type", content_type)
        .body(Body::from(body.to_vec()))
        .expect("request");
    let response = call(router, request).await;
    let status = response.status();
    let value = json_body(response).await;
    assert!(
        status.is_success(),
        "upload of {bucket}/{key} failed with {status}: {value}"
    );
    value
}

// ---------------------------------------------------------------------------
// Cluster fixtures
//
// The cluster routes report on a real consensus group. A single member is still
// a genuine group: it elects itself, commits through the durable log, and
// applies to the real catalog, so these tests exercise the same code a
// multi-node deployment runs. Only the peer transport is a stub, because a lone
// member has nobody to call — and if it ever tried, these tests fail loudly
// rather than pretending the call succeeded.
// ---------------------------------------------------------------------------

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use record_store_consensus::{
    ConsensusSettings, MemberId, MemberNode, MetadataConsensus, RecordStoreTypeConfig,
    ReplicatedClusterStore, ReplicatedMetadataRepository,
};
use record_store_core::{Checksum, NodeId, ObjectId};
use record_store_replication::{
    ClusterContext, ClusterOperations, Coordinator, CoordinatorSettings, TaskHealth,
};
use record_store_rpc::{
    RemoteReadStream, RemoteReplicaVerification, RemoteReplicaWrite, ReplicaTarget,
    ReplicaTransport, RpcClientError, TransferExpectation, TransferStream,
};

use crate::ClusterManagement;

struct UnreachablePeers;
struct NoPeer;

fn no_peers() -> std::io::Error {
    std::io::Error::other("a single-member cluster has no peers to contact")
}

impl RaftNetwork<RecordStoreTypeConfig> for NoPeer {
    async fn append_entries(
        &mut self,
        _request: AppendEntriesRequest<RecordStoreTypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>>
    {
        Err(RPCError::Unreachable(Unreachable::new(&no_peers())))
    }

    async fn vote(
        &mut self,
        _request: VoteRequest<MemberId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<MemberId>, RPCError<MemberId, MemberNode, RaftError<MemberId>>> {
        Err(RPCError::Unreachable(Unreachable::new(&no_peers())))
    }

    async fn install_snapshot(
        &mut self,
        _request: InstallSnapshotRequest<RecordStoreTypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<MemberId>,
        RPCError<MemberId, MemberNode, RaftError<MemberId, InstallSnapshotError>>,
    > {
        Err(RPCError::Network(NetworkError::new(&no_peers())))
    }
}

impl RaftNetworkFactory<RecordStoreTypeConfig> for UnreachablePeers {
    type Network = NoPeer;

    async fn new_client(&mut self, _target: MemberId, _node: &MemberNode) -> Self::Network {
        NoPeer
    }
}

/// A transport whose every call fails, because there are no peers to reach.
struct NoTransport;

fn unreachable_peer() -> RpcClientError {
    RpcClientError::Unreachable {
        address: "no-peer".to_owned(),
        reason: "a single-member cluster has no peers".to_owned(),
    }
}

#[async_trait::async_trait]
impl ReplicaTransport for NoTransport {
    async fn write_replica(
        &self,
        _target: &ReplicaTarget,
        _operation_id: &str,
        _object_id: ObjectId,
        _expectation: TransferExpectation,
        _body: TransferStream,
    ) -> Result<RemoteReplicaWrite, RpcClientError> {
        Err(unreachable_peer())
    }

    async fn read_replica(
        &self,
        _target: &ReplicaTarget,
        _object_id: ObjectId,
        _size: u64,
        _checksum: &Checksum,
    ) -> Result<RemoteReadStream, RpcClientError> {
        Err(unreachable_peer())
    }

    async fn delete_replica(
        &self,
        _target: &ReplicaTarget,
        _object_id: ObjectId,
    ) -> Result<bool, RpcClientError> {
        Err(unreachable_peer())
    }

    async fn verify_replica(
        &self,
        _target: &ReplicaTarget,
        _object_id: ObjectId,
        _size: u64,
        _checksum: &Checksum,
    ) -> Result<RemoteReplicaVerification, RpcClientError> {
        Err(unreachable_peer())
    }

    async fn list_local_payloads(
        &self,
        _target: &ReplicaTarget,
        _after: Option<ObjectId>,
        _limit: usize,
    ) -> Result<Vec<ObjectId>, RpcClientError> {
        Err(unreachable_peer())
    }
}

/// Builds the management router with cluster administration wired in.
pub(crate) async fn clustered_api() -> (TempDir, Router) {
    let directory = tempfile::tempdir().expect("temporary directory");

    let mut settings =
        ConsensusSettings::new(1, "127.0.0.1:17603", directory.path().join("consensus"));
    settings.heartbeat_interval_millis = 20;
    settings.election_timeout_min_millis = 60;
    settings.election_timeout_max_millis = 120;

    let consensus = MetadataConsensus::start(settings, UnreachablePeers)
        .await
        .expect("start consensus");
    consensus
        .initialize_single_member()
        .await
        .expect("initialize");
    consensus
        .wait_for_leader(std::time::Duration::from_secs(10))
        .await
        .expect("elect a leader");

    let metadata: Arc<dyn MetadataRepository> =
        Arc::new(ReplicatedMetadataRepository::new(Arc::clone(&consensus)));
    let storage: Arc<dyn ObjectStore> = Arc::new(
        LocalFilesystemStore::open(
            directory.path().join("data"),
            directory.path().join("tmp"),
            Arc::clone(&metadata),
        )
        .await
        .expect("filesystem store"),
    );
    let audit: Arc<dyn AuditRepository> = Arc::new(
        RedbAuditRepository::open(directory.path().join("audit.redb"))
            .await
            .expect("audit repository"),
    );
    let credentials = Arc::new(
        CredentialManager::open(
            directory.path().join("credentials.redb"),
            "root-access-key",
            "root-secret-at-least-sixteen",
            Some(b"credential-master-key-at-least-32-bytes"),
        )
        .await
        .expect("credential manager"),
    );
    let owner = OrganizationId::new();
    let services = Services::new(
        Arc::clone(&storage),
        Arc::clone(&metadata),
        owner,
        ServiceLimits {
            maximum_concurrent_operations: 8,
            maximum_custom_metadata_entries: 8,
            maximum_custom_metadata_bytes: 1_024,
        },
    );

    // A cluster-mode server forms its one-member group before serving traffic,
    // so the fixture does the same: without this the routes correctly report an
    // uninitialized cluster and nothing else can be exercised.
    let node_id = NodeId::new();
    let cluster: Arc<dyn record_store_consensus::ClusterStore> =
        Arc::new(ReplicatedClusterStore::new(Arc::clone(&consensus)));
    cluster
        .apply(record_store_cluster::ClusterCommand::InitializeCluster {
            identity: record_store_cluster::ClusterIdentity {
                cluster_id: record_store_core::ClusterId::new(),
                cluster_format_version: record_store_cluster::CLUSTER_FORMAT_VERSION,
                created_at: chrono::Utc::now(),
            },
            config: Box::new(record_store_cluster::ClusterConfig::default()),
        })
        .await
        .expect("initialize the cluster");
    cluster
        .apply(record_store_cluster::ClusterCommand::RegisterNode {
            registration: Box::new(record_store_cluster::NodeRegistration {
                node_id,
                versions: record_store_cluster::NodeVersions::current("test"),
                rpc_address: "127.0.0.1:17603".to_owned(),
                s3_endpoint: None,
                storage_class: record_store_cluster::StorageClass::new("standard")
                    .expect("storage class"),
                failure_domain: record_store_cluster::FailureDomain::default(),
                capacity: record_store_cluster::NodeCapacity::default(),
                devices: Vec::new(),
                started_at: chrono::Utc::now(),
            }),
            at: chrono::Utc::now(),
        })
        .await
        .expect("register this node");

    let context = Arc::new(ClusterContext {
        node_id,
        cluster: Arc::clone(&cluster),
        metadata: Arc::clone(&metadata),
        local: Arc::new(DeviceStore::single(
            record_store_cluster::DeviceRecord::legacy_id(node_id),
            Arc::new(
                LocalFilesystemStore::open(
                    directory.path().join("replicas"),
                    directory.path().join("replica-tmp"),
                    Arc::clone(&metadata),
                )
                .await
                .expect("replica store"),
            ),
        )),
        transport: Arc::new(NoTransport),
        placement: Arc::new(record_store_cluster::CapacityAwarePlacement::new(None)),
        consensus: Some(Arc::clone(&consensus)),
    });
    let coordinator = Arc::new(Coordinator::new(
        Arc::clone(&context),
        Arc::clone(&consensus),
        CoordinatorSettings::default(),
    ));
    let operations = Arc::new(ClusterOperations::new(
        Arc::clone(&context),
        coordinator,
        Arc::clone(&consensus),
    ));

    let state = AppState::new(
        storage,
        metadata,
        services,
        credentials,
        audit,
        owner,
        "0.0.0-test",
    )
    .with_mode(record_store_config::DeploymentMode::Cluster)
    .with_cluster(ClusterManagement::new(
        context,
        consensus,
        operations,
        Arc::new(TaskHealth::default()),
    ))
    .with_management_auth(ManagementAuth::bearer_tokens(
        SYSTEM_TOKEN.as_bytes(),
        Some(STORAGE_TOKEN.as_bytes()),
        Some(AUDITOR_TOKEN.as_bytes()),
    ))
    // Without this the scrape endpoint answers 401 in cluster mode, which left
    // every cluster and device metric untested: the only harness that has a
    // cluster could not reach /metrics.
    .with_metrics_auth(MetricsAuth::bearer_token(METRICS_TOKEN.as_bytes()));
    (directory, router(state))
}
