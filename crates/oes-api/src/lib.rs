//! Native operational and authenticated management HTTP API.

use std::{
    fmt::{self, Display, Formatter},
    future::{Future, IntoFuture},
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header, header::HeaderName},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use oes_audit::{AuditEvent, AuditQuery, AuditRepository, AuditResult};
use oes_auth::{CredentialManager, Policy, PolicyStatement, ServiceAccountInfo};
use oes_cluster::ClusterOperationKind;
use oes_config::DeploymentMode;
use oes_consensus::MetadataConsensus;
use oes_core::{
    Bucket, BucketName, BucketQuota, ClusterId, ExpirationDays, LifecycleRule, LifecycleRuleId,
    NodeId, ObjectKey, ObjectMetadata, OrganizationId, ServiceAccountId, StorageUsage, VersionId,
    VersioningState, WebhookId,
};
use oes_events::{
    CreateWebhookRequest, CreatedWebhook, EventRepository, WebhookDeliveryLog, WebhookSubscription,
};
use oes_metadata::MetadataRepository;
use oes_replication::{
    ClusterContext, ClusterOperations, ClusterStatus, OperationError, TaskHealth,
};
use oes_service::{ServiceError, ServiceListRequest, Services};
use oes_storage::{
    ObjectStore, StorageInspection, StorageRepairRequest, StorageRepairResult, StorageStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{net::TcpListener, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};
use uuid::Uuid;

static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Explicit dependencies shared by native HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    storage: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataRepository>,
    services: Services,
    credentials: Arc<CredentialManager>,
    audit: Arc<dyn AuditRepository>,
    owner: OrganizationId,
    version: &'static str,
    mode: DeploymentMode,
    management_auth: ManagementAuth,
    metrics_auth: MetricsAuth,
    events: Option<Arc<dyn EventRepository>>,
    cluster: Option<ClusterManagement>,
}

/// Cluster services exposed through the authenticated management API.
#[derive(Clone)]
pub struct ClusterManagement {
    context: Arc<ClusterContext>,
    consensus: Arc<MetadataConsensus>,
    operations: Arc<ClusterOperations>,
    task_health: Arc<TaskHealth>,
}

impl ClusterManagement {
    /// Creates a cluster management surface from running cluster services.
    #[must_use]
    pub const fn new(
        context: Arc<ClusterContext>,
        consensus: Arc<MetadataConsensus>,
        operations: Arc<ClusterOperations>,
        task_health: Arc<TaskHealth>,
    ) -> Self {
        Self {
            context,
            consensus,
            operations,
            task_health,
        }
    }

    async fn status(&self) -> Result<ClusterStatus, String> {
        ClusterStatus::collect(&self.context, &self.consensus, self.task_health.snapshot()).await
    }
}

impl AppState {
    /// Constructs application state without hidden global dependencies.
    #[must_use]
    pub fn new(
        storage: Arc<dyn ObjectStore>,
        metadata: Arc<dyn MetadataRepository>,
        services: Services,
        credentials: Arc<CredentialManager>,
        audit: Arc<dyn AuditRepository>,
        owner: OrganizationId,
        version: &'static str,
    ) -> Self {
        let management_auth = ManagementAuth::legacy_root(Arc::clone(&credentials));
        Self {
            storage,
            metadata,
            services,
            credentials,
            audit,
            owner,
            version,
            mode: DeploymentMode::Standalone,
            management_auth,
            metrics_auth: MetricsAuth::disabled(),
            events: None,
            cluster: None,
        }
    }

    /// Records how this process participates in a deployment.
    #[must_use]
    pub const fn with_mode(mut self, mode: DeploymentMode) -> Self {
        self.mode = mode;
        self
    }

    /// Replaces legacy root Basic authentication with dedicated management tokens.
    #[must_use]
    pub fn with_management_auth(mut self, management_auth: ManagementAuth) -> Self {
        self.management_auth = management_auth;
        self
    }

    /// Enables Prometheus scraping with a credential independent of management roles.
    #[must_use]
    pub fn with_metrics_auth(mut self, metrics_auth: MetricsAuth) -> Self {
        self.metrics_auth = metrics_auth;
        self
    }

    /// Adds durable storage events and webhook administration.
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn EventRepository>) -> Self {
        self.events = Some(events);
        self
    }

    /// Adds cluster status and administration to this API instance.
    #[must_use]
    pub fn with_cluster(mut self, cluster: ClusterManagement) -> Self {
        self.cluster = Some(cluster);
        self
    }

    async fn check_ready(&self) -> Result<(), ReadinessError> {
        tokio::try_join!(
            async {
                self.storage
                    .check_ready()
                    .await
                    .map_err(ReadinessError::Storage)
            },
            async {
                self.metadata
                    .check_ready()
                    .await
                    .map_err(ReadinessError::Metadata)
            },
            async {
                self.audit
                    .check_ready()
                    .await
                    .map_err(ReadinessError::Audit)
            },
            async {
                if let Some(events) = &self.events {
                    events.check_ready().await.map_err(ReadinessError::Events)?;
                }
                Ok(())
            },
        )?;
        if let Some(cluster) = &self.cluster {
            let status = cluster.status().await.map_err(ReadinessError::Cluster)?;
            if !status.metadata.status.readable
                || !status
                    .nodes
                    .iter()
                    .any(|node| node.node_id == cluster.context.node_id)
            {
                return Err(ReadinessError::Cluster(
                    "local node has no usable metadata membership".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// The authenticated management identity, returned to clients after sign-in.
#[derive(Debug, Clone, Serialize)]
struct SessionResponse {
    role: ManagementRole,
    /// Coarse permissions the role grants.
    ///
    /// Clients use these to hide actions that would be refused. They are a
    /// usability aid only: the API enforces every permission independently.
    permissions: RolePermissions,
}

/// What a management role is allowed to do.
#[derive(Debug, Clone, Copy, Serialize)]
struct RolePermissions {
    manage_buckets: bool,
    manage_objects: bool,
    manage_service_accounts: bool,
    manage_policies: bool,
    manage_webhooks: bool,
    read_audit: bool,
    manage_cluster: bool,
    manage_storage: bool,
}

impl RolePermissions {
    const fn of(role: ManagementRole) -> Self {
        match role {
            ManagementRole::SystemAdministrator => Self {
                manage_buckets: true,
                manage_objects: true,
                manage_service_accounts: true,
                manage_policies: true,
                manage_webhooks: true,
                read_audit: true,
                manage_cluster: true,
                manage_storage: true,
            },
            ManagementRole::StorageAdministrator => Self {
                manage_buckets: true,
                manage_objects: true,
                manage_service_accounts: false,
                manage_policies: false,
                manage_webhooks: false,
                read_audit: false,
                manage_cluster: false,
                manage_storage: true,
            },
            ManagementRole::Auditor => Self {
                manage_buckets: false,
                manage_objects: false,
                manage_service_accounts: false,
                manage_policies: false,
                manage_webhooks: false,
                read_audit: true,
                manage_cluster: false,
                manage_storage: false,
            },
        }
    }
}

/// Coarse management roles kept separate from S3 policy actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementRole {
    /// Full credential, policy, storage, and audit administration.
    SystemAdministrator,
    /// Bucket, object, quota, repair, and integrity administration.
    StorageAdministrator,
    /// Read-only access to operational metadata and the audit trail.
    Auditor,
}

#[derive(Clone)]
struct ManagementToken {
    digest: [u8; 32],
    role: ManagementRole,
}

/// Dedicated bearer-token authentication for the native management plane.
#[derive(Clone)]
pub struct ManagementAuth {
    tokens: Arc<[ManagementToken]>,
    legacy_root: Option<Arc<CredentialManager>>,
}

/// Authentication dedicated to the Prometheus scrape endpoint.
///
/// Metrics are closed when no token is configured. The scrape credential has
/// no authority on management routes.
#[derive(Clone)]
pub struct MetricsAuth {
    digest: Option<[u8; 32]>,
}

impl MetricsAuth {
    /// Creates an enabled metrics authenticator from one bearer token.
    #[must_use]
    pub fn bearer_token(token: &[u8]) -> Self {
        Self {
            digest: Some(Sha256::digest(token).into()),
        }
    }

    const fn disabled() -> Self {
        Self { digest: None }
    }

    fn authenticate(&self, request: &Request) -> bool {
        let Some(expected) = self.digest else {
            return false;
        };
        let Some(token) = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return false;
        };
        let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(expected.ct_eq(&actual))
    }
}

impl ManagementAuth {
    /// Creates a token set. At least the system-administrator token is expected
    /// for a production deployment; optional role tokens can be omitted.
    #[must_use]
    pub fn bearer_tokens(
        system_administrator: &[u8],
        storage_administrator: Option<&[u8]>,
        auditor: Option<&[u8]>,
    ) -> Self {
        let mut tokens = vec![ManagementToken::new(
            system_administrator,
            ManagementRole::SystemAdministrator,
        )];
        if let Some(token) = storage_administrator {
            tokens.push(ManagementToken::new(
                token,
                ManagementRole::StorageAdministrator,
            ));
        }
        if let Some(token) = auditor {
            tokens.push(ManagementToken::new(token, ManagementRole::Auditor));
        }
        Self {
            tokens: tokens.into(),
            legacy_root: None,
        }
    }

    fn legacy_root(credentials: Arc<CredentialManager>) -> Self {
        Self {
            tokens: Arc::from([]),
            legacy_root: Some(credentials),
        }
    }

    fn authenticate(&self, request: &Request) -> Option<ManagementPrincipal> {
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?;
        if let Some(token) = authorization.strip_prefix("Bearer ") {
            let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
            return self.tokens.iter().find_map(|candidate| {
                bool::from(candidate.digest.ct_eq(&digest)).then_some(ManagementPrincipal {
                    role: candidate.role,
                })
            });
        }
        let credentials = self.legacy_root.as_ref()?;
        let encoded = authorization.strip_prefix("Basic ")?;
        let decoded = STANDARD.decode(encoded).ok()?;
        let delimiter = decoded.iter().position(|byte| *byte == b':')?;
        let access = std::str::from_utf8(&decoded[..delimiter]).ok()?;
        credentials
            .verify_root(access, &decoded[delimiter + 1..])
            .then_some(ManagementPrincipal {
                role: ManagementRole::SystemAdministrator,
            })
    }
}

impl ManagementToken {
    fn new(token: &[u8], role: ManagementRole) -> Self {
        Self {
            digest: Sha256::digest(token).into(),
            role,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ManagementPrincipal {
    role: ManagementRole,
}

impl ManagementPrincipal {
    fn permits(self, request: &Request) -> bool {
        let path = request.uri().path();
        match self.role {
            ManagementRole::SystemAdministrator => true,
            ManagementRole::StorageAdministrator => {
                let cluster_mutation = request.method() != axum::http::Method::GET
                    && (path.starts_with("/api/v1/cluster")
                        || path.starts_with("/api/v1/nodes")
                        || path.starts_with("/api/v1/rebalance")
                        || path.starts_with("/api/v1/repair"));
                !cluster_mutation
                    && !path.starts_with("/api/v1/service-accounts")
                    && !path.starts_with("/api/v1/policies")
                    && !path.starts_with("/api/v1/audit")
                    && !path.starts_with("/api/v1/webhooks")
                    && !path.starts_with("/api/v1/webhook-deliveries")
            }
            ManagementRole::Auditor => {
                request.method() == axum::http::Method::GET
                    && (path == "/api/v1/auth/session"
                        || path == "/api/v1/system/info"
                        || path == "/api/v1/events"
                        || path == "/api/v1/audit/events"
                        || path == "/api/v1/storage/status"
                        || path == "/api/v1/storage/usage"
                        || path == "/api/v1/storage/inspect"
                        || path == "/api/v1/buckets"
                        || path == "/api/v1/webhooks"
                        || path == "/api/v1/webhook-deliveries"
                        || path.starts_with("/api/v1/cluster")
                        || path.starts_with("/api/v1/nodes")
                        || path.starts_with("/api/v1/repair")
                        || path.starts_with("/api/v1/rebalance"))
            }
        }
    }

    const fn audit_name(self) -> &'static str {
        match self.role {
            ManagementRole::SystemAdministrator => "management:system-administrator",
            ManagementRole::StorageAdministrator => "management:storage-administrator",
            ManagementRole::Auditor => "management:auditor",
        }
    }
}

/// Builds public operational routes and authenticated administrative routes.
pub fn router(state: AppState) -> Router {
    let administrative = Router::new()
        .route("/api/v1/system/info", get(system_info))
        .route("/api/v1/system/metrics", get(system_metrics))
        .route("/api/v1/auth/session", get(auth_session))
        .route("/api/v1/storage/status", get(storage_status))
        .route("/api/v1/storage/usage", get(storage_usage))
        .route("/api/v1/storage/inspect", get(storage_inspect))
        .route("/api/v1/cluster", get(cluster_status))
        .route(
            "/api/v1/cluster/init",
            axum::routing::post(cluster_initialize),
        )
        .route("/api/v1/cluster/health", get(cluster_health))
        .route(
            "/api/v1/cluster/join-tokens",
            axum::routing::post(issue_cluster_join_token),
        )
        .route("/api/v1/nodes", get(list_cluster_nodes))
        .route("/api/v1/nodes/{id}", get(inspect_cluster_node))
        .route(
            "/api/v1/nodes/{id}/drain",
            axum::routing::post(drain_cluster_node),
        )
        .route(
            "/api/v1/nodes/{id}/maintenance",
            axum::routing::post(maintain_cluster_node),
        )
        .route(
            "/api/v1/nodes/{id}/resume",
            axum::routing::post(resume_cluster_node),
        )
        .route(
            "/api/v1/nodes/{id}/decommission",
            axum::routing::post(decommission_cluster_node),
        )
        .route("/api/v1/repair/status", get(repair_status))
        .route("/api/v1/rebalance", axum::routing::post(start_rebalance))
        .route("/api/v1/rebalance/status", get(rebalance_status))
        .route(
            "/api/v1/storage/repair",
            axum::routing::post(storage_repair),
        )
        .route("/api/v1/audit/events", get(list_audit_events))
        .route("/api/v1/webhooks", get(list_webhooks).post(create_webhook))
        .route("/api/v1/webhooks/{id}", delete(delete_webhook))
        .route(
            "/api/v1/webhooks/{id}/status",
            axum::routing::put(set_webhook_status),
        )
        .route("/api/v1/webhook-deliveries", get(list_webhook_deliveries))
        .route("/api/v1/buckets", get(list_buckets).post(create_bucket))
        .route("/api/v1/buckets/{bucket}", delete(delete_bucket))
        .route("/api/v1/buckets/{bucket}/objects", get(list_bucket_objects))
        .route(
            "/api/v1/buckets/{bucket}/object/{*key}",
            get(get_bucket_object)
                .delete(delete_bucket_object)
                .put(upload_bucket_object)
                // Object bodies are streamed, so the shared small-payload limit
                // that protects JSON routes must not apply here.
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/v1/buckets/{bucket}/object-content/{*key}",
            get(download_bucket_object),
        )
        .route(
            "/api/v1/buckets/{bucket}/object-versions",
            get(list_bucket_object_versions),
        )
        .route(
            "/api/v1/buckets/{bucket}/object-versions/{*key}",
            delete(delete_bucket_object_version),
        )
        .route("/api/v1/events", get(list_storage_events))
        .route(
            "/api/v1/buckets/{bucket}/versioning",
            get(get_bucket_versioning).put(set_bucket_versioning),
        )
        .route(
            "/api/v1/buckets/{bucket}/quota",
            axum::routing::put(set_bucket_quota),
        )
        .route(
            "/api/v1/buckets/{bucket}/lifecycle",
            get(list_lifecycle_rules).post(create_lifecycle_rule),
        )
        .route(
            "/api/v1/buckets/{bucket}/lifecycle/{rule_id}",
            axum::routing::put(update_lifecycle_rule),
        )
        .route(
            "/api/v1/lifecycle-rules/{id}",
            delete(delete_lifecycle_rule),
        )
        .route(
            "/api/v1/service-accounts",
            get(list_service_accounts).post(create_service_account),
        )
        .route(
            "/api/v1/service-accounts/{id}",
            get(get_service_account).delete(delete_service_account),
        )
        .route(
            "/api/v1/service-accounts/{id}/status",
            axum::routing::put(set_service_account_status),
        )
        .route(
            "/api/v1/service-accounts/{id}/credentials",
            axum::routing::post(rotate_credential),
        )
        .route(
            "/api/v1/service-accounts/{id}/temporary-credentials",
            axum::routing::post(issue_temporary_credential),
        )
        .route(
            "/api/v1/service-accounts/{id}/credentials/{credential_id}/status",
            axum::routing::put(set_credential_status),
        )
        .route("/api/v1/policies", get(list_policies).post(create_policy))
        .route(
            "/api/v1/policies/{policy_id}/bindings/{account_id}",
            axum::routing::put(attach_policy).delete(detach_policy),
        )
        .route(
            "/api/v1/restore/{bucket}/{*key}",
            axum::routing::post(restore_version),
        )
        .route(
            "/api/v1/buckets/{bucket}/object-copy/{*key}",
            axum::routing::post(copy_object),
        )
        .route(
            "/api/v1/verify/objects/{bucket}/{*key}",
            axum::routing::post(verify_object),
        )
        .route(
            "/api/v1/verify/buckets/{bucket}",
            axum::routing::post(verify_bucket),
        )
        .route_layer(middleware::from_fn_with_state(
            state.management_auth.clone(),
            require_management,
        ));
    let operational_metrics =
        Router::new()
            .route("/metrics", get(metrics))
            .route_layer(middleware::from_fn_with_state(
                state.metrics_auth.clone(),
                require_metrics,
            ));

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .merge(operational_metrics)
        .merge(administrative)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_context,
        ))
        .with_state(state)
}

/// Serves until shutdown is requested, then drains requests up to `grace_period`.
pub async fn serve<F>(
    listener: TcpListener,
    application: Router,
    shutdown: F,
    grace_period: Duration,
) -> Result<(), ServerError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let cancellation = CancellationToken::new();
    let graceful = axum::serve(
        listener,
        application.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(cancellation.clone().cancelled_owned())
    .into_future();
    tokio::pin!(graceful);
    tokio::select! {
        result = &mut graceful => result.map_err(ServerError::Serve),
        () = shutdown => {
            cancellation.cancel();
            match timeout(grace_period, &mut graceful).await {
                Ok(result) => result.map_err(ServerError::Serve),
                Err(_) => Err(ServerError::ShutdownTimeout(grace_period)),
            }
        }
    }
}

async fn require_management(
    State(authentication): State<ManagementAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::new);
    let Some(principal) = authentication.authenticate(&request) else {
        return ApiError::unauthorized(request_id).into_response();
    };
    if !principal.permits(&request) {
        return ApiError::forbidden(request_id).into_response();
    }
    request.extensions_mut().insert(principal);
    let mut response = next.run(request).await;
    response.extensions_mut().insert(principal);
    response
}

async fn require_metrics(
    State(authentication): State<MetricsAuth>,
    request: Request,
    next: Next,
) -> Response {
    if !authentication.authenticate(&request) {
        let request_id = request
            .extensions()
            .get::<RequestId>()
            .cloned()
            .unwrap_or_else(RequestId::new);
        return ApiError::unauthorized(request_id).into_response();
    }
    next.run(request).await
}

async fn health() -> Json<StatusResponse> {
    Json(StatusResponse { status: "ok" })
}

async fn ready(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StatusResponse>, ApiError> {
    ensure_ready(&state, request_id).await?;
    Ok(Json(StatusResponse { status: "ready" }))
}

async fn system_info(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<SystemInfoResponse>, ApiError> {
    ensure_ready(&state, request_id.clone()).await?;
    let cluster_id = match &state.cluster {
        Some(_) => Some(collect_cluster_status(&state, request_id).await?.cluster_id),
        None => None,
    };
    let capabilities = Capabilities::detect(&state);
    Ok(Json(SystemInfoResponse {
        name: "oes",
        version: state.version,
        status: "ready",
        mode: state.mode,
        cluster_id,
        capabilities,
    }))
}

async fn cluster_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ClusterStatus>, ApiError> {
    Ok(Json(collect_cluster_status(&state, request_id).await?))
}

async fn cluster_initialize(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ClusterStatus>, ApiError> {
    // Cluster-mode servers form and persist their initial one-member consensus
    // group before accepting HTTP traffic. Keeping this endpoint idempotent
    // gives operators one stable `oes cluster init` workflow without allowing a
    // second cluster identity to be created accidentally.
    Ok(Json(collect_cluster_status(&state, request_id).await?))
}

#[derive(Serialize)]
struct ClusterHealthResponse {
    health: oes_cluster::ClusterHealth,
    reasons: Vec<String>,
    metadata: oes_consensus::MetadataQuorum,
    data: oes_cluster::DataHealth,
}

async fn cluster_health(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ClusterHealthResponse>, ApiError> {
    let status = collect_cluster_status(&state, request_id).await?;
    Ok(Json(ClusterHealthResponse {
        health: status.health,
        reasons: status.reasons(),
        metadata: status.metadata,
        data: status.data,
    }))
}

async fn list_cluster_nodes(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<oes_replication::NodeStatus>>, ApiError> {
    Ok(Json(
        collect_cluster_status(&state, request_id).await?.nodes,
    ))
}

async fn inspect_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<oes_replication::NodeStatus>, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    collect_cluster_status(&state, request_id.clone())
        .await?
        .nodes
        .into_iter()
        .find(|node| node.node_id == node_id)
        .map(Json)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "NODE_NOT_FOUND",
                format!("Node {node_id} is not a member of this cluster"),
                request_id,
            )
        })
}

async fn drain_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<oes_cluster::ClusterOperation>, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    let operation = cluster_management(&state, request_id.clone())?
        .operations
        .drain(node_id)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(Json(operation))
}

async fn maintain_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    cluster_management(&state, request_id.clone())?
        .operations
        .maintenance(node_id)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    cluster_management(&state, request_id.clone())?
        .operations
        .resume(node_id)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Default, Deserialize)]
struct DecommissionInput {
    #[serde(default)]
    force: bool,
}

async fn decommission_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    input: Option<Json<DecommissionInput>>,
) -> Result<Json<oes_cluster::ClusterOperation>, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    let force = input.map(|Json(input)| input.force).unwrap_or_default();
    let operation = cluster_management(&state, request_id.clone())?
        .operations
        .decommission(node_id, force)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(Json(operation))
}

async fn repair_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<oes_replication::RepairStatus>, ApiError> {
    Ok(Json(
        collect_cluster_status(&state, request_id).await?.repair,
    ))
}

async fn start_rebalance(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<oes_cluster::ClusterOperation>, ApiError> {
    let operation = cluster_management(&state, request_id.clone())?
        .operations
        .rebalance()
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(Json(operation))
}

async fn rebalance_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<oes_cluster::ClusterOperation>>, ApiError> {
    let operations = collect_cluster_status(&state, request_id)
        .await?
        .operations
        .into_iter()
        .filter(|operation| operation.kind == ClusterOperationKind::Rebalance)
        .collect();
    Ok(Json(operations))
}

#[derive(Deserialize)]
struct JoinTokenInput {
    #[serde(default = "default_join_token_lifetime")]
    lifetime_seconds: u64,
    #[serde(default)]
    description: String,
}

const fn default_join_token_lifetime() -> u64 {
    3_600
}

#[derive(Serialize)]
struct IssuedJoinTokenResponse {
    id: oes_core::JoinTokenId,
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

async fn issue_cluster_join_token(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<JoinTokenInput>,
) -> Result<(StatusCode, Json<IssuedJoinTokenResponse>), ApiError> {
    if !(oes_cluster::JoinToken::MINIMUM_LIFETIME_SECONDS
        ..=oes_cluster::JoinToken::MAXIMUM_LIFETIME_SECONDS)
        .contains(&input.lifetime_seconds)
    {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_JOIN_TOKEN_LIFETIME",
            "Join token lifetime must be between 60 and 86400 seconds",
        ));
    }
    let issued = cluster_management(&state, request_id.clone())?
        .operations
        .issue_join_token(input.lifetime_seconds, input.description)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedJoinTokenResponse {
            id: issued.record.id,
            token: issued.token.expose().to_owned(),
            expires_at: issued.record.expires_at,
        }),
    ))
}

fn cluster_management(
    state: &AppState,
    request_id: RequestId,
) -> Result<&ClusterManagement, ApiError> {
    state.cluster.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "CLUSTER_MODE_DISABLED",
            "This OES process is running in standalone mode",
            request_id,
        )
    })
}

async fn collect_cluster_status(
    state: &AppState,
    request_id: RequestId,
) -> Result<ClusterStatus, ApiError> {
    cluster_management(state, request_id.clone())?
        .status()
        .await
        .map_err(|error| {
            error!(request_id = %request_id, %error, "cluster status collection failed");
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "CLUSTER_UNAVAILABLE",
                error,
                request_id,
            )
        })
}

fn parse_node_id(value: &str, request_id: RequestId) -> Result<NodeId, ApiError> {
    value.parse().map_err(|_| {
        ApiError::bad_request(
            request_id,
            "INVALID_NODE_ID",
            "Node ID must be a valid OES node identifier",
        )
    })
}

fn cluster_operation_error(error_value: OperationError, request_id: RequestId) -> ApiError {
    let status = match error_value {
        OperationError::NodeNotFound(_) => StatusCode::NOT_FOUND,
        OperationError::InvalidTransition { .. } | OperationError::DurabilityAtRisk(_) => {
            StatusCode::CONFLICT
        }
        OperationError::Cluster(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    let code = match error_value {
        OperationError::NodeNotFound(_) => "NODE_NOT_FOUND",
        OperationError::InvalidTransition { .. } => "INVALID_NODE_TRANSITION",
        OperationError::DurabilityAtRisk(_) => "DURABILITY_AT_RISK",
        OperationError::Cluster(_) => "CLUSTER_UNAVAILABLE",
    };
    ApiError::new(status, code, error_value.to_string(), request_id)
}

/// Returns the identity behind the presented management credential.
///
/// A console calls this immediately after sign-in: a `401` means the credential
/// is not usable, and a success tells it which actions to offer.
async fn auth_session(
    Extension(principal): Extension<ManagementPrincipal>,
) -> Json<SessionResponse> {
    Json(SessionResponse {
        role: principal.role,
        permissions: RolePermissions::of(principal.role),
    })
}

/// Lists objects under a prefix, one bounded page at a time.
///
/// Listing is always paginated: a bucket may hold millions of objects, so no
/// caller is ever handed the whole keyspace.
async fn list_bucket_objects(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Query(query): Query<ObjectListQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectListResponse>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    if query.limit == 0 || query.limit > 1_000 {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_LIMIT",
            "limit must be between 1 and 1000",
        ));
    }
    let start_after = match &query.continuation_token {
        Some(token) => Some(decode_cursor(token, &request_id)?),
        None => None,
    };
    let result = state
        .services
        .objects
        .list(ServiceListRequest {
            bucket: name,
            prefix: query.prefix,
            delimiter: query.delimiter,
            maximum_keys: query.limit,
            start_after,
        })
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(ObjectListResponse {
        objects: result
            .objects
            .into_iter()
            .map(ObjectSummary::from)
            .collect(),
        prefixes: result.common_prefixes.into_iter().collect(),
        is_truncated: result.is_truncated,
        next_continuation_token: result.next_marker.as_deref().map(encode_cursor),
    }))
}

/// Returns one object's metadata without transferring its bytes.
async fn get_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectSummary>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    state
        .services
        .objects
        .head(&name, key)
        .await
        .map(|metadata| Json(ObjectSummary::from(metadata)))
        .map_err(|error| service_to_api_error(error, request_id))
}

/// Streams an object's bytes to the caller.
///
/// The payload is streamed rather than buffered, so object size is bounded by
/// storage rather than by this process's memory.
async fn download_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    let result = state
        .services
        .objects
        .get(&name, key.clone(), None)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let content_type = result
        .metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let filename = key
        .as_str()
        .rsplit('/')
        .next()
        .unwrap_or("download")
        .to_owned();
    let mut response = Response::new(axum::body::Body::from_stream(result.body));
    let headers = response.headers_mut();
    insert_header(headers, header::CONTENT_TYPE, &content_type);
    insert_header(
        headers,
        header::CONTENT_LENGTH,
        &result.metadata.size.to_string(),
    );
    insert_header(
        headers,
        header::ETAG,
        &format!("\"{}\"", result.metadata.etag.as_str()),
    );
    // The filename is quoted and escaped so a key containing quotes cannot
    // break out of the header value.
    insert_header(
        headers,
        header::CONTENT_DISPOSITION,
        &format!(
            "attachment; filename=\"{}\"",
            filename.replace(['\\', '"'], "_")
        ),
    );
    Ok(response)
}

/// Streams an uploaded object into storage.
async fn upload_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    request: Request,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let object_key = parse_object_key(&key, &request_id)?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = request.into_body().into_data_stream();
    let stream = futures_util::TryStreamExt::map_err(body, std::io::Error::other);
    let result = state
        .services
        .objects
        .put(oes_service::ServicePutRequest {
            bucket: name,
            key: object_key,
            content_type,
            custom_metadata: std::collections::BTreeMap::new(),
            expected_checksum: None,
            body: oes_storage::upload_stream(stream),
        })
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(ObjectSummary::from(result.metadata)),
    ))
}

/// Deletes the visible version of an object.
async fn delete_bucket_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    let removed = state
        .services
        .objects
        .delete(&name, key)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_NOT_FOUND",
            "Object was not found",
            request_id,
        ))
    }
}

/// Lists the version history under a prefix.
async fn list_bucket_object_versions(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Query(query): Query<ObjectVersionListQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectVersionListResponse>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    if query.limit == 0 || query.limit > 1_000 {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_LIMIT",
            "limit must be between 1 and 1000",
        ));
    }
    if query.key_marker.is_some() != query.version_id_marker.is_some() {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_VERSION_CURSOR",
            "Both version cursor fields are required",
        ));
    }
    let result = state
        .services
        .objects
        .list_versions(oes_service::ServiceListVersionsRequest {
            bucket: name,
            prefix: query.prefix,
            key_marker: query.key_marker,
            version_id_marker: query.version_id_marker,
            maximum_keys: query.limit,
        })
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(ObjectVersionListResponse {
        versions: result
            .versions
            .into_iter()
            .map(|listed| {
                let is_latest = listed.is_latest;
                match listed.record {
                    oes_core::ObjectVersionRecord::Object { metadata, is_null } => {
                        ObjectVersionEntry {
                            key: metadata.key.to_string(),
                            version_id: metadata.version_id,
                            is_latest,
                            is_delete_marker: false,
                            is_null,
                            created_at: metadata.created_at,
                            size: Some(metadata.size),
                            etag: Some(metadata.etag.as_str().to_owned()),
                            checksum: Some(metadata.checksum.to_string()),
                        }
                    }
                    oes_core::ObjectVersionRecord::DeleteMarker { marker, is_null } => {
                        ObjectVersionEntry {
                            key: marker.key.to_string(),
                            version_id: marker.version_id,
                            is_latest,
                            is_delete_marker: true,
                            is_null,
                            created_at: marker.created_at,
                            size: None,
                            etag: None,
                            checksum: None,
                        }
                    }
                }
            })
            .collect(),
        next_key_marker: result.next_key_marker,
        next_version_id_marker: result.next_version_id_marker,
    }))
}

/// Permanently removes one object version.
async fn delete_bucket_object_version(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<DeleteVersionQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    state
        .services
        .objects
        .delete_version(&name, key, query.version_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| service_to_api_error(error, request_id))
}

/// Returns recent storage events, newest first.
///
/// Storage events record what happened to data. They are intentionally a
/// different feed from the audit trail, which records who requested it.
async fn list_storage_events(
    State(state): State<AppState>,
    Query(query): Query<EventQueryParameters>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<EventsResponse>, ApiError> {
    let events = event_repository(&state, &request_id)?;
    let after = match (query.after_time, query.after_id) {
        (Some(time), Some(id)) => Some((time, id)),
        (None, None) => None,
        _ => {
            return Err(ApiError::bad_request(
                request_id,
                "INVALID_EVENT_CURSOR",
                "Both event cursor fields are required",
            ));
        }
    };
    let page = events
        .list_events(oes_events::EventQuery {
            since: query.since,
            until: query.until,
            bucket: query.bucket,
            event_type: query.event_type,
            object_prefix: query.prefix,
            after,
            limit: query.limit,
        })
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "storage event query failed");
            ApiError::internal(request_id)
        })?;
    let (next_time, next_id) = page
        .next
        .map_or((None, None), |(time, id)| (Some(time), Some(id)));
    Ok(Json(EventsResponse {
        events: page.events,
        next_time,
        next_id,
    }))
}

fn insert_header(headers: &mut header::HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn parse_bucket_name(value: &str, request_id: &RequestId) -> Result<BucketName, ApiError> {
    BucketName::new(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })
}

fn parse_object_key(value: &str, request_id: &RequestId) -> Result<ObjectKey, ApiError> {
    ObjectKey::new(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })
}

/// Pagination cursors are opaque so clients cannot build on their internals.
fn encode_cursor(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value.as_bytes())
}

fn decode_cursor(value: &str, request_id: &RequestId) -> Result<String, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_CONTINUATION_TOKEN",
            "Invalid continuation token",
        )
    };
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    String::from_utf8(decoded).map_err(|_| invalid())
}

async fn storage_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageStatusResponse>, ApiError> {
    state
        .services
        .objects
        .status()
        .await
        .map(StorageStatusResponse::from)
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

async fn storage_usage(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageUsage>, ApiError> {
    state
        .services
        .objects
        .usage()
        .await
        .map(Json)
        .map_err(|error| internal_service_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct StorageInspectionQuery {
    #[serde(default = "default_inspection_limit")]
    maximum_entries: usize,
}

const fn default_inspection_limit() -> usize {
    100_000
}

async fn storage_inspect(
    State(state): State<AppState>,
    Query(query): Query<StorageInspectionQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<StorageInspection>, ApiError> {
    state
        .services
        .objects
        .inspect(query.maximum_entries)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct StorageRepairInput {
    #[serde(default = "default_inspection_limit")]
    maximum_entries: usize,
    #[serde(default = "dry_run_by_default")]
    dry_run: bool,
}

const fn dry_run_by_default() -> bool {
    true
}

async fn storage_repair(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StorageRepairInput>,
) -> Result<Json<StorageRepairResult>, ApiError> {
    state
        .services
        .objects
        .repair(StorageRepairRequest {
            maximum_entries: input.maximum_entries,
            dry_run: input.dry_run,
        })
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn list_buckets(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<BucketSummary>>, ApiError> {
    let buckets = state
        .services
        .buckets
        .list()
        .await
        .map_err(|error| internal_service_error(error, request_id.clone()))?;
    // Usage for every bucket is read in one pass so rendering a bucket table
    // never turns into one request per row.
    let usage = state.metadata.bucket_usage().await.map_err(|error| {
        error!(%error, request_id = %request_id, "bucket usage lookup failed");
        ApiError::internal(request_id)
    })?;
    Ok(Json(
        buckets
            .into_iter()
            .map(|bucket| {
                let counters = usage.get(&bucket.id).copied().unwrap_or_default();
                BucketSummary {
                    bucket,
                    object_count: counters.object_count,
                    logical_bytes: counters.logical_bytes,
                    version_count: counters.version_count,
                    version_bytes: counters.version_bytes,
                    multipart_bytes: counters.multipart_bytes,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct CreateBucketRequest {
    name: String,
}

async fn create_bucket(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateBucketRequest>,
) -> Result<(StatusCode, Json<Bucket>), ApiError> {
    let name = BucketName::new(input.name).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .create(name)
        .await
        .map(|bucket| (StatusCode::CREATED, Json(bucket)))
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn delete_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn list_service_accounts(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<ServiceAccountInfo>>, ApiError> {
    state
        .credentials
        .list_service_accounts()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential operation failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct CreateServiceAccountRequest {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Serialize)]
struct IssuedServiceAccountResponse {
    account: oes_auth::ServiceAccount,
    credential: oes_auth::Credential,
    secret_access_key: String,
}

async fn create_service_account(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateServiceAccountRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    let issued = state
        .credentials
        .create_service_account_with_description(input.name, input.description, state.owner)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential issuance failed");
            ApiError::bad_request(
                request_id.clone(),
                "INVALID_SERVICE_ACCOUNT",
                "Invalid service account",
            )
        })?;
    let secret_access_key =
        String::from_utf8(issued.secret.expose().to_vec()).map_err(|error| {
            error!(%error, request_id = %request_id, "generated credential was not UTF-8");
            ApiError::internal(request_id)
        })?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

async fn get_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ServiceAccountInfo>, ApiError> {
    let id = ServiceAccountId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })?;
    state
        .credentials
        .get_service_account(id)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account lookup failed");
            ApiError::internal(request_id)
        })
}

async fn delete_service_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    state
        .credentials
        .delete_service_account(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account deletion failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct StatusChangeRequest {
    enabled: bool,
}

async fn set_service_account_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StatusChangeRequest>,
) -> Result<Json<ServiceAccountInfo>, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    state
        .credentials
        .set_service_account_enabled(id, input.enabled)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "service account status update failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct RotateCredentialRequest {
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn rotate_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RotateCredentialRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    let issued = state
        .credentials
        .rotate_credential(id, input.expires_at)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential rotation failed");
            ApiError::internal(request_id.clone())
        })?;
    let secret_access_key = String::from_utf8(issued.secret.expose().to_vec())
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

#[derive(Debug, Deserialize)]
struct TemporaryCredentialRequest {
    expires_in_seconds: u64,
}

async fn issue_temporary_credential(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<TemporaryCredentialRequest>,
) -> Result<(StatusCode, Json<IssuedServiceAccountResponse>), ApiError> {
    if !(60..=86_400).contains(&input.expires_in_seconds) {
        return Err(ApiError::bad_request(
            request_id,
            "INVALID_EXPIRATION",
            "Temporary credentials must expire between 60 and 86400 seconds",
        ));
    }
    let id = parse_service_account_id(&id, &request_id)?;
    let seconds = i64::try_from(input.expires_in_seconds).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_EXPIRATION",
            "Temporary credential expiration is invalid",
        )
    })?;
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(seconds);
    let issued = state
        .credentials
        .rotate_credential(id, Some(expires_at))
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "temporary credential issuance failed");
            ApiError::internal(request_id.clone())
        })?;
    let secret_access_key = String::from_utf8(issued.secret.expose().to_vec())
        .map_err(|_| ApiError::internal(request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedServiceAccountResponse {
            account: issued.info.account,
            credential: issued.info.credential,
            secret_access_key,
        }),
    ))
}

async fn set_credential_status(
    State(state): State<AppState>,
    Path((id, credential_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<StatusChangeRequest>,
) -> Result<StatusCode, ApiError> {
    let id = parse_service_account_id(&id, &request_id)?;
    let credential_id = Uuid::parse_str(&credential_id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_CREDENTIAL_ID",
            "Invalid credential ID",
        )
    })?;
    state
        .credentials
        .set_credential_enabled(id, credential_id, input.enabled)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "credential status update failed");
            ApiError::internal(request_id)
        })
}

fn parse_service_account_id(
    value: &str,
    request_id: &RequestId,
) -> Result<ServiceAccountId, ApiError> {
    ServiceAccountId::from_str(value).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_SERVICE_ACCOUNT_ID",
            "Invalid service account ID",
        )
    })
}

async fn get_bucket_versioning(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<VersioningResponse>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id))?;
    Ok(Json(VersioningResponse {
        versioning: bucket.versioning,
    }))
}

#[derive(Serialize)]
struct VersioningResponse {
    versioning: VersioningState,
}

#[derive(Deserialize)]
struct SetVersioningRequest {
    versioning: VersioningState,
}

async fn set_bucket_versioning(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<SetVersioningRequest>,
) -> Result<Json<Bucket>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .set_versioning(&name, input.versioning)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Deserialize)]
struct SetQuotaRequest {
    quota: BucketQuota,
}

async fn set_bucket_quota(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<SetQuotaRequest>,
) -> Result<Json<Bucket>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    state
        .services
        .buckets
        .set_quota(&name, input.quota)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct CreateLifecycleRuleRequest {
    #[serde(default)]
    prefix: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    expiration: Option<ExpirationDays>,
    noncurrent_version_expiration: Option<ExpirationDays>,
}

const fn enabled_by_default() -> bool {
    true
}

async fn create_lifecycle_rule(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateLifecycleRuleRequest>,
) -> Result<(StatusCode, Json<LifecycleRule>), ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let now = chrono::Utc::now();
    let rule = LifecycleRule {
        id: LifecycleRuleId::new(),
        bucket_id: bucket.id,
        prefix: input.prefix,
        enabled: input.enabled,
        expiration: input.expiration,
        noncurrent_version_expiration: input.noncurrent_version_expiration,
        created_at: now,
        updated_at: now,
    };
    rule.validate().map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE",
            "Lifecycle rule is invalid",
        )
    })?;
    state
        .metadata
        .put_lifecycle_rule(&rule)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule creation failed");
            ApiError::internal(request_id)
        })?;
    Ok((StatusCode::CREATED, Json(rule)))
}

async fn list_lifecycle_rules(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<LifecycleRule>>, ApiError> {
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    state
        .metadata
        .list_lifecycle_rules(Some(bucket.id))
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule listing failed");
            ApiError::internal(request_id)
        })
}

/// A complete replacement for one lifecycle rule.
///
/// Every field is sent, so clearing an expiration is expressed as an explicit
/// null rather than being indistinguishable from "leave this alone". The console
/// already holds the whole rule from the listing, so there is nothing to gain
/// from a partial update and a real ambiguity to avoid.
#[derive(Debug, Deserialize)]
struct UpdateLifecycleRuleRequest {
    prefix: String,
    enabled: bool,
    #[serde(default)]
    expiration: Option<ExpirationDays>,
    #[serde(default)]
    noncurrent_version_expiration: Option<ExpirationDays>,
}

/// Replaces one lifecycle rule belonging to a bucket.
///
/// The rule is addressed through its bucket so a rule identifier from one bucket
/// cannot be used to edit another's, and so the lookup stays bounded by that
/// bucket's rule count.
async fn update_lifecycle_rule(
    State(state): State<AppState>,
    Path((bucket, rule_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<UpdateLifecycleRuleRequest>,
) -> Result<Json<LifecycleRule>, ApiError> {
    let name = parse_bucket_name(&bucket, &request_id)?;
    let rule_id = LifecycleRuleId::from_str(&rule_id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE_ID",
            "Invalid lifecycle rule ID",
        )
    })?;
    let bucket = state
        .services
        .buckets
        .head(&name)
        .await
        .map_err(|error| service_to_api_error(error, request_id.clone()))?;
    let existing = state
        .metadata
        .list_lifecycle_rules(Some(bucket.id))
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule listing failed");
            ApiError::internal(request_id.clone())
        })?
        .into_iter()
        .find(|rule| rule.id == rule_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "LIFECYCLE_RULE_NOT_FOUND",
                "Lifecycle rule was not found in this bucket",
                request_id.clone(),
            )
        })?;

    let updated = LifecycleRule {
        id: existing.id,
        bucket_id: existing.bucket_id,
        prefix: input.prefix,
        enabled: input.enabled,
        expiration: input.expiration,
        noncurrent_version_expiration: input.noncurrent_version_expiration,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now(),
    };
    updated.validate().map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE",
            "Lifecycle rule is invalid",
        )
    })?;
    state
        .metadata
        .put_lifecycle_rule(&updated)
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule update failed");
            ApiError::internal(request_id)
        })?;
    Ok(Json(updated))
}

async fn delete_lifecycle_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = LifecycleRuleId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_LIFECYCLE_RULE_ID",
            "Invalid lifecycle rule ID",
        )
    })?;
    state
        .metadata
        .delete_lifecycle_rule(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "lifecycle rule deletion failed");
            ApiError::bad_request(
                request_id,
                "LIFECYCLE_RULE_NOT_FOUND",
                "Lifecycle rule was not found",
            )
        })
}

#[derive(Deserialize)]
struct CreatePolicyRequest {
    name: String,
    #[serde(default)]
    description: String,
    statements: Vec<PolicyStatement>,
}

async fn create_policy(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<Policy>), ApiError> {
    state
        .credentials
        .create_policy(input.name, input.description, input.statements)
        .await
        .map(|policy| (StatusCode::CREATED, Json(policy)))
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy creation failed");
            ApiError::bad_request(request_id, "INVALID_POLICY", "Invalid policy")
        })
}

async fn list_policies(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<Policy>>, ApiError> {
    state
        .credentials
        .list_policies()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy listing failed");
            ApiError::internal(request_id)
        })
}

async fn attach_policy(
    State(state): State<AppState>,
    Path((policy_id, account_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let policy_id = Uuid::parse_str(&policy_id).map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_POLICY_ID", "Invalid policy ID")
    })?;
    let account_id = parse_service_account_id(&account_id, &request_id)?;
    state
        .credentials
        .attach_policy(account_id, policy_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy attachment failed");
            ApiError::internal(request_id)
        })
}

async fn detach_policy(
    State(state): State<AppState>,
    Path((policy_id, account_id)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let policy_id = Uuid::parse_str(&policy_id).map_err(|_| {
        ApiError::bad_request(request_id.clone(), "INVALID_POLICY_ID", "Invalid policy ID")
    })?;
    let account_id = parse_service_account_id(&account_id, &request_id)?;
    state
        .credentials
        .detach_policy(account_id, policy_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "policy detachment failed");
            ApiError::internal(request_id)
        })
}

#[derive(Deserialize)]
struct RestoreVersionRequest {
    version_id: VersionId,
}

async fn restore_version(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<RestoreVersionRequest>,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let bucket = parse_bucket_name(&bucket, &request_id)?;
    let key = parse_object_key(&key, &request_id)?;
    state
        .services
        .objects
        .restore_version(&bucket, key, input.version_id)
        .await
        .map(|result| {
            (
                StatusCode::CREATED,
                Json(ObjectSummary::from(result.metadata)),
            )
        })
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Debug, Deserialize)]
struct CopyObjectRequest {
    /// Bucket the bytes are read from.
    source_bucket: String,
    /// Key the bytes are read from.
    source_key: String,
    /// Optional historical version to copy instead of the current one.
    #[serde(default)]
    source_version_id: Option<VersionId>,
    /// Replacement media type. Supplying any replacement field replaces the
    /// source's metadata rather than carrying it across.
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    custom_metadata: Option<std::collections::BTreeMap<String, String>>,
}

/// Copies an object server side.
///
/// The path names the destination, because that is what the request creates.
/// Bytes are streamed by the service layer and never buffered here, so copying
/// a large object costs the API process nothing beyond the transfer itself.
async fn copy_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CopyObjectRequest>,
) -> Result<(StatusCode, Json<ObjectSummary>), ApiError> {
    let destination_bucket = parse_bucket_name(&bucket, &request_id)?;
    let destination_key = parse_object_key(&key, &request_id)?;
    let source_bucket = parse_bucket_name(&input.source_bucket, &request_id)?;
    let source_key = parse_object_key(&input.source_key, &request_id)?;
    let replaces_metadata = input.content_type.is_some() || input.custom_metadata.is_some();
    state
        .services
        .objects
        .copy(oes_service::ServiceCopyRequest {
            source_bucket,
            source_key,
            source_version_id: input.source_version_id,
            destination_bucket,
            destination_key,
            metadata_directive: if replaces_metadata {
                oes_service::CopyMetadataDirective::Replace
            } else {
                oes_service::CopyMetadataDirective::Copy
            },
            content_type: input.content_type,
            replacement_metadata: input.custom_metadata.unwrap_or_default(),
        })
        .await
        .map(|result| {
            (
                StatusCode::CREATED,
                Json(ObjectSummary::from(result.metadata)),
            )
        })
        .map_err(|error| service_to_api_error(error, request_id))
}

async fn verify_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ObjectMetadata>, ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let key = ObjectKey::new(key).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })?;
    state
        .services
        .objects
        .verify(&bucket, key)
        .await
        .map(Json)
        .map_err(|error| service_to_api_error(error, request_id))
}

#[derive(Serialize)]
struct VerifyBucketResponse {
    verified_objects: u64,
    failures: u64,
}

async fn verify_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<VerifyBucketResponse>, ApiError> {
    let bucket = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let mut marker = None;
    let mut verified = 0_u64;
    let mut failures = 0_u64;
    loop {
        let page = state
            .services
            .objects
            .list(ServiceListRequest {
                bucket: bucket.clone(),
                prefix: String::new(),
                delimiter: None,
                maximum_keys: 1_000,
                start_after: marker.clone(),
            })
            .await
            .map_err(|error| service_to_api_error(error, request_id.clone()))?;
        for object in &page.objects {
            if state
                .services
                .objects
                .verify(&bucket, object.key.clone())
                .await
                .is_ok()
            {
                verified += 1;
            } else {
                failures += 1;
            }
        }
        if !page.is_truncated {
            break;
        }
        marker = page.next_marker;
    }
    Ok(Json(VerifyBucketResponse {
        verified_objects: verified,
        failures,
    }))
}

/// Everything both metric representations are built from.
///
/// Gathering once and rendering twice is what keeps the Prometheus exposition
/// and the console's JSON view from drifting apart. Prometheus scrapes with a
/// dedicated credential; the console reads the same numbers with a management
/// token, because it must never hold the scrape token.
#[derive(Debug, Serialize)]
struct MetricsSnapshot {
    /// Requests served since this process started.
    requests: u64,
    /// Requests that failed since this process started.
    errors: u64,
    /// Bytes accepted from clients since this process started.
    upload_bytes: u64,
    /// Bytes served to clients since this process started.
    download_bytes: u64,
    storage: StorageMetrics,
    /// Present only in cluster mode, so a standalone console shows no cluster
    /// figures rather than zeroes that look like a broken cluster.
    #[serde(skip_serializing_if = "Option::is_none")]
    cluster: Option<ClusterMetrics>,
}

#[derive(Debug, Serialize)]
struct StorageMetrics {
    object_count: u64,
    bucket_count: u64,
    version_count: u64,
    logical_bytes: u64,
    physical_bytes: u64,
    multipart_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ClusterMetrics {
    nodes: u64,
    healthy: bool,
    quorum_writable: bool,
    under_replicated_objects: u64,
    /// Repair tasks currently running. Exposed to Prometheus under the older
    /// name `oes_replication_queue_depth`, which is kept for existing scrapers.
    repair_active_tasks: u64,
    node_capacity_bytes: u64,
    node_used_bytes: u64,
    node_available_bytes: u64,
    logical_bytes: u64,
    physical_bytes: u64,
}

/// Collects the current metric values.
async fn gather_metrics(
    state: &AppState,
    request_id: &RequestId,
) -> Result<MetricsSnapshot, ApiError> {
    let metrics = state.services.metrics.snapshot();
    let usage = state
        .services
        .objects
        .usage()
        .await
        .map_err(|error| internal_service_error(error, request_id.clone()))?;

    let mut cluster_metrics = None;
    if let Some(cluster) = &state.cluster {
        match cluster.status().await {
            Ok(status) => {
                let local = status
                    .nodes
                    .iter()
                    .find(|node| node.node_id == cluster.context.node_id);
                let capacity = local.map_or(0, |node| node.capacity_bytes);
                let available = local.map_or(0, |node| node.available_bytes);
                cluster_metrics = Some(ClusterMetrics {
                    nodes: status.nodes.len() as u64,
                    healthy: status.health == oes_cluster::ClusterHealth::Healthy,
                    quorum_writable: status.metadata.status.writable,
                    under_replicated_objects: status.replication.under_replicated_payloads,
                    repair_active_tasks: status.repair.active_tasks,
                    node_capacity_bytes: capacity,
                    node_used_bytes: capacity.saturating_sub(available),
                    node_available_bytes: available,
                    logical_bytes: status.replication.logical_bytes,
                    physical_bytes: status.replication.physical_bytes,
                });
            }
            Err(error) => {
                // A cluster read failure must not fail the whole scrape; the
                // process-level counters are still worth reporting.
                error!(%error, "cluster metrics snapshot could not be collected");
            }
        }
    }

    Ok(MetricsSnapshot {
        requests: metrics.requests,
        errors: metrics.errors,
        upload_bytes: metrics.upload_bytes,
        download_bytes: metrics.download_bytes,
        storage: StorageMetrics {
            object_count: usage.object_count,
            bucket_count: usage.bucket_count,
            version_count: usage.version_count,
            logical_bytes: usage.bytes_used,
            physical_bytes: usage.physical_bytes,
            multipart_bytes: usage.temporary_multipart_bytes,
        },
        cluster: cluster_metrics,
    })
}

/// Renders one snapshot as Prometheus text exposition.
fn prometheus_exposition(snapshot: &MetricsSnapshot) -> String {
    let mut body = String::new();
    let mut gauge = |name: &str, kind: &str, value: u64| {
        body.push_str(&format!("# TYPE {name} {kind}\n{name} {value}\n"));
    };
    gauge("oes_s3_requests_total", "counter", snapshot.requests);
    gauge("oes_requests_total", "counter", snapshot.requests);
    gauge("oes_errors_total", "counter", snapshot.errors);
    gauge("oes_objects_total", "gauge", snapshot.storage.object_count);
    gauge("oes_storage_bytes", "gauge", snapshot.storage.logical_bytes);
    gauge(
        "oes_versions_total",
        "gauge",
        snapshot.storage.version_count,
    );
    gauge("oes_buckets_total", "gauge", snapshot.storage.bucket_count);
    gauge(
        "oes_storage_logical_bytes",
        "gauge",
        snapshot.storage.logical_bytes,
    );
    gauge(
        "oes_storage_physical_bytes",
        "gauge",
        snapshot.storage.physical_bytes,
    );
    gauge(
        "oes_multipart_bytes",
        "gauge",
        snapshot.storage.multipart_bytes,
    );
    gauge("oes_upload_bytes_total", "counter", snapshot.upload_bytes);
    gauge(
        "oes_download_bytes_total",
        "counter",
        snapshot.download_bytes,
    );
    if let Some(cluster) = &snapshot.cluster {
        gauge(
            "oes_node_capacity_bytes",
            "gauge",
            cluster.node_capacity_bytes,
        );
        gauge("oes_node_used_bytes", "gauge", cluster.node_used_bytes);
        gauge(
            "oes_node_available_bytes",
            "gauge",
            cluster.node_available_bytes,
        );
        gauge("oes_node_health", "gauge", u64::from(cluster.healthy));
        gauge("oes_cluster_nodes", "gauge", cluster.nodes);
        gauge(
            "oes_under_replicated_objects",
            "gauge",
            cluster.under_replicated_objects,
        );
        gauge(
            "oes_replication_queue_depth",
            "gauge",
            cluster.repair_active_tasks,
        );
        gauge(
            "oes_metadata_quorum_health",
            "gauge",
            u64::from(cluster.quorum_writable),
        );
        gauge("oes_cluster_logical_bytes", "gauge", cluster.logical_bytes);
        gauge(
            "oes_cluster_physical_bytes",
            "gauge",
            cluster.physical_bytes,
        );
    }
    body
}

async fn metrics(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Response, ApiError> {
    let snapshot = gather_metrics(&state, &request_id).await?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        prometheus_exposition(&snapshot),
    )
        .into_response())
}

/// Serves the same metric values as JSON for the management plane.
///
/// The console cannot read `/metrics`: that endpoint takes the dedicated scrape
/// credential, which the console deliberately does not hold. This route carries
/// the same numbers behind management authentication instead.
async fn system_metrics(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<MetricsSnapshot>, ApiError> {
    gather_metrics(&state, &request_id).await.map(Json)
}

async fn create_webhook(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<CreateWebhookRequest>,
) -> Result<(StatusCode, Json<CreatedWebhook>), ApiError> {
    event_repository(&state, &request_id)?
        .create_webhook(input)
        .await
        .map(|created| (StatusCode::CREATED, Json(created)))
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook creation failed");
            ApiError::bad_request(
                request_id,
                "INVALID_WEBHOOK",
                "Webhook configuration is invalid or disallowed",
            )
        })
}

async fn list_webhooks(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<WebhookSubscription>>, ApiError> {
    event_repository(&state, &request_id)?
        .list_webhooks()
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook listing failed");
            ApiError::internal(request_id)
        })
}

#[derive(Debug, Deserialize)]
struct WebhookStatusRequest {
    enabled: bool,
}

async fn set_webhook_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<WebhookStatusRequest>,
) -> Result<Json<WebhookSubscription>, ApiError> {
    let id = WebhookId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_WEBHOOK_ID",
            "Invalid webhook ID",
        )
    })?;
    event_repository(&state, &request_id)?
        .set_webhook_enabled(id, input.enabled)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook status update failed");
            ApiError::bad_request(request_id, "WEBHOOK_NOT_FOUND", "Webhook was not found")
        })
}

async fn delete_webhook(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let id = WebhookId::from_str(&id).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_WEBHOOK_ID",
            "Invalid webhook ID",
        )
    })?;
    event_repository(&state, &request_id)?
        .delete_webhook(id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook deletion failed");
            ApiError::bad_request(request_id, "WEBHOOK_NOT_FOUND", "Webhook was not found")
        })
}

#[derive(Debug, Deserialize)]
struct DeliveryLogQuery {
    #[serde(default = "default_delivery_limit")]
    limit: usize,
}

const fn default_delivery_limit() -> usize {
    100
}

async fn list_webhook_deliveries(
    State(state): State<AppState>,
    Query(query): Query<DeliveryLogQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<WebhookDeliveryLog>>, ApiError> {
    event_repository(&state, &request_id)?
        .list_delivery_logs(query.limit)
        .await
        .map(Json)
        .map_err(|error| {
            error!(%error, request_id = %request_id, "webhook delivery log query failed");
            ApiError::bad_request(
                request_id,
                "INVALID_DELIVERY_QUERY",
                "Delivery query is invalid",
            )
        })
}

fn event_repository<'a>(
    state: &'a AppState,
    request_id: &RequestId,
) -> Result<&'a Arc<dyn EventRepository>, ApiError> {
    state
        .events
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable(request_id.clone()))
}

#[derive(Debug, Deserialize)]
struct AuditQueryParameters {
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    principal: Option<String>,
    operation: Option<String>,
    resource: Option<String>,
    result: Option<AuditResult>,
    source_ip: Option<String>,
    request_id: Option<String>,
    after_time: Option<chrono::DateTime<chrono::Utc>>,
    after_id: Option<oes_core::AuditEventId>,
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

const fn default_audit_limit() -> usize {
    100
}

#[derive(Serialize)]
struct AuditEventsResponse {
    events: Vec<AuditEvent>,
    next_time: Option<chrono::DateTime<chrono::Utc>>,
    next_id: Option<oes_core::AuditEventId>,
}

async fn list_audit_events(
    State(state): State<AppState>,
    Query(query): Query<AuditQueryParameters>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<AuditEventsResponse>, ApiError> {
    let after = match (query.after_time, query.after_id) {
        (Some(time), Some(id)) => Some((time, id)),
        (None, None) => None,
        _ => {
            return Err(ApiError::bad_request(
                request_id,
                "INVALID_AUDIT_CURSOR",
                "Both audit cursor fields are required",
            ));
        }
    };
    let page = state
        .audit
        .query(AuditQuery {
            since: query.since,
            until: query.until,
            principal: query.principal,
            operation: query.operation,
            resource_prefix: query.resource,
            result: query.result,
            source_ip: query.source_ip,
            request_id: query.request_id,
            after,
            limit: query.limit,
        })
        .await
        .map_err(|error| {
            error!(%error, request_id = %request_id, "audit query failed");
            ApiError::bad_request(request_id, "INVALID_AUDIT_QUERY", "Invalid audit query")
        })?;
    let (next_time, next_id) = page
        .next
        .map_or((None, None), |(time, id)| (Some(time), Some(id)));
    Ok(Json(AuditEventsResponse {
        events: page.events,
        next_time,
        next_id,
    }))
}

async fn ensure_ready(state: &AppState, request_id: RequestId) -> Result<(), ApiError> {
    if let Err(error_value) = state.check_ready().await {
        error!(request_id = %request_id, error = %error_value, "readiness check failed");
        return Err(ApiError::service_unavailable(request_id));
    }
    Ok(())
}

async fn not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::not_found(request_id)
}

async fn request_context(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = request
        .headers()
        .get(&REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(RequestId::accept)
        .unwrap_or_else(RequestId::new);
    request.extensions_mut().insert(request_id.clone());
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let source_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip().to_string());
    let started = Instant::now();
    let span =
        info_span!("http.request", request_id = %request_id, method = %method, route = %path);
    let mut response = next.run(request).instrument(span.clone()).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    info!(parent: &span, status = response.status().as_u16(), latency_ms, "request completed");
    if let Ok(value) = HeaderValue::from_str(request_id.as_str()) {
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER.clone(), value);
    }
    if path.starts_with("/api/v1/") {
        let result = match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AuditResult::Denied,
            status if status.is_success() => AuditResult::Success,
            _ => AuditResult::Failure,
        };
        let event = AuditEvent {
            event_id: oes_core::AuditEventId::new(),
            timestamp: chrono::Utc::now(),
            request_id: Some(request_id.to_string()),
            principal: response
                .extensions()
                .get::<ManagementPrincipal>()
                .copied()
                .map_or(
                    "management:unauthenticated",
                    ManagementPrincipal::audit_name,
                )
                .into(),
            credential_id: None,
            source_ip,
            operation: format!("{} {}", method, path),
            resource: path,
            result,
            metadata: Default::default(),
        };
        if let Err(error) = state.audit.append(&event).await {
            error!(%error, request_id = %request_id, "durable audit append failed");
        }
    }
    response
}

/// A validated request correlation identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestId(String);

impl RequestId {
    fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    fn accept(value: &str) -> Option<Self> {
        (!value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte)))
        .then(|| Self(value.to_owned()))
    }

    /// Returns the validated identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for RequestId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct SystemInfoResponse {
    name: &'static str,
    version: &'static str,
    status: &'static str,
    mode: DeploymentMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    cluster_id: Option<ClusterId>,
    capabilities: Capabilities,
}

/// What this deployment can actually do.
///
/// Clients use this instead of inferring behaviour from a version number, so a
/// build that lacks a capability simply reports it as unavailable.
#[derive(Debug, Clone, Copy, Serialize)]
struct Capabilities {
    /// Cluster membership, replication, repair, and rebalancing are available.
    cluster: bool,
    /// Bucket versioning can be enabled and versions can be listed.
    versioning: bool,
    /// Signed outbound storage-event webhooks are available.
    webhooks: bool,
    /// Storage-event history can be queried.
    events: bool,
    /// Metadata-driven object expiration is available.
    lifecycle: bool,
    /// Object bytes can be browsed and transferred through this API.
    object_browser: bool,
    /// Erasure coding is not implemented; replication is the durability model.
    erasure_coding: bool,
}

impl Capabilities {
    fn detect(state: &AppState) -> Self {
        Self {
            cluster: state.cluster.is_some(),
            versioning: true,
            webhooks: state.events.is_some(),
            events: state.events.is_some(),
            lifecycle: true,
            object_browser: true,
            erasure_coding: false,
        }
    }
}

/// A bucket with the accounting a console needs to render a table.
///
/// Usage is included here so listing buckets costs one request rather than one
/// request per bucket.
#[derive(Debug, Serialize)]
struct BucketSummary {
    #[serde(flatten)]
    bucket: Bucket,
    object_count: u64,
    logical_bytes: u64,
    version_count: u64,
    version_bytes: u64,
    multipart_bytes: u64,
}

/// Object metadata safe to expose to a management client.
///
/// Internal payload identifiers and physical representation are deliberately
/// omitted: where an object physically lives is not a client concern.
#[derive(Debug, Serialize)]
struct ObjectSummary {
    key: String,
    size: u64,
    content_type: Option<String>,
    etag: String,
    checksum: String,
    version_id: VersionId,
    created_at: chrono::DateTime<chrono::Utc>,
    modified_at: chrono::DateTime<chrono::Utc>,
    custom_metadata: std::collections::BTreeMap<String, String>,
}

impl From<ObjectMetadata> for ObjectSummary {
    fn from(value: ObjectMetadata) -> Self {
        Self {
            key: value.key.to_string(),
            size: value.size,
            content_type: value.content_type,
            etag: value.etag.as_str().to_owned(),
            checksum: value.checksum.to_string(),
            version_id: value.version_id,
            created_at: value.created_at,
            modified_at: value.modified_at,
            custom_metadata: value.custom_metadata,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ObjectListQuery {
    #[serde(default)]
    prefix: String,
    delimiter: Option<String>,
    continuation_token: Option<String>,
    #[serde(default = "default_object_limit")]
    limit: usize,
}

const fn default_object_limit() -> usize {
    100
}

/// One page of a prefix listing.
///
/// Prefixes are logical groupings derived from the delimiter; OES stores no
/// directories, so they are reported separately from objects rather than being
/// presented as entries of the same kind.
#[derive(Debug, Serialize)]
struct ObjectListResponse {
    objects: Vec<ObjectSummary>,
    prefixes: Vec<String>,
    is_truncated: bool,
    next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ObjectVersionListQuery {
    #[serde(default)]
    prefix: String,
    key_marker: Option<String>,
    version_id_marker: Option<VersionId>,
    #[serde(default = "default_object_limit")]
    limit: usize,
}

/// One version-history entry.
#[derive(Debug, Serialize)]
struct ObjectVersionEntry {
    key: String,
    version_id: VersionId,
    is_latest: bool,
    is_delete_marker: bool,
    /// Whether S3 exposes this entry as the special unversioned entry.
    is_null: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    size: Option<u64>,
    etag: Option<String>,
    checksum: Option<String>,
}

#[derive(Debug, Serialize)]
struct ObjectVersionListResponse {
    versions: Vec<ObjectVersionEntry>,
    next_key_marker: Option<String>,
    next_version_id_marker: Option<VersionId>,
}

#[derive(Debug, Deserialize)]
struct DeleteVersionQuery {
    version_id: VersionId,
}

#[derive(Debug, Deserialize)]
struct EventQueryParameters {
    since: Option<chrono::DateTime<chrono::Utc>>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    bucket: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<oes_events::StorageEventType>,
    prefix: Option<String>,
    after_time: Option<chrono::DateTime<chrono::Utc>>,
    after_id: Option<oes_core::EventId>,
    #[serde(default = "default_audit_limit")]
    limit: usize,
}

#[derive(Debug, Serialize)]
struct EventsResponse {
    events: Vec<oes_events::StorageEvent>,
    next_time: Option<chrono::DateTime<chrono::Utc>>,
    next_id: Option<oes_core::EventId>,
}

#[derive(Debug, Serialize)]
struct StorageStatusResponse {
    capacity_bytes: u64,
    available_bytes: u64,
    temporary_upload_bytes: u64,
}

impl From<StorageStatus> for StorageStatusResponse {
    fn from(value: StorageStatus) -> Self {
        Self {
            capacity_bytes: value.capacity_bytes,
            available_bytes: value.available_bytes,
            temporary_upload_bytes: value.temporary_upload_bytes,
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    request_id: RequestId,
}

impl ApiError {
    fn not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "ROUTE_NOT_FOUND",
            "The requested route was not found",
            request_id,
        )
    }

    fn unauthorized(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "Authentication is required",
            request_id,
        )
    }

    fn forbidden(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "The management role does not permit this operation",
            request_id,
        )
    }

    fn service_unavailable(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_NOT_READY",
            "The service is not ready",
            request_id,
        )
    }

    fn internal(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "An internal error occurred",
            request_id,
        )
    }

    fn bad_request(request_id: RequestId, code: &'static str, message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, request_id)
    }

    fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        request_id: RequestId,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            request_id,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                    request_id: self.request_id.to_string(),
                },
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    request_id: String,
}

fn service_to_api_error(error: ServiceError, request_id: RequestId) -> ApiError {
    match error {
        ServiceError::BucketNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "BUCKET_NOT_FOUND",
            "Bucket was not found",
            request_id,
        ),
        ServiceError::BucketAlreadyExists => ApiError::new(
            StatusCode::CONFLICT,
            "BUCKET_ALREADY_EXISTS",
            "Bucket already exists",
            request_id,
        ),
        ServiceError::BucketNotEmpty => ApiError::new(
            StatusCode::CONFLICT,
            "BUCKET_NOT_EMPTY",
            "Bucket is not empty",
            request_id,
        ),
        ServiceError::ObjectNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_NOT_FOUND",
            "Object was not found",
            request_id,
        ),
        // A delete marker is a real version that deliberately hides the object.
        // Reporting it distinctly from a missing key tells the caller history
        // exists and can be restored.
        ServiceError::DeleteMarker(_) => ApiError::new(
            StatusCode::NOT_FOUND,
            "OBJECT_DELETED",
            "The object's current version is a delete marker",
            request_id,
        ),
        ServiceError::MultipartUploadNotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "MULTIPART_UPLOAD_NOT_FOUND",
            "Multipart upload was not found",
            request_id,
        ),
        ServiceError::QuotaExceeded => ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "QUOTA_EXCEEDED",
            "The storage quota would be exceeded",
            request_id,
        ),
        ServiceError::Core(_)
        | ServiceError::InvalidRequest(_)
        | ServiceError::MetadataTooLarge => {
            ApiError::bad_request(request_id, "INVALID_REQUEST", "Invalid request")
        }
        error => internal_service_error(error, request_id),
    }
}

fn internal_service_error(error_value: ServiceError, request_id: RequestId) -> ApiError {
    error!(request_id = %request_id, error = %error_value, "management operation failed");
    ApiError::internal(request_id)
}

#[derive(Debug, Error)]
enum ReadinessError {
    #[error("storage dependency failed: {0}")]
    Storage(oes_storage::StorageError),
    #[error("metadata dependency failed: {0}")]
    Metadata(oes_metadata::MetadataError),
    #[error("audit dependency failed: {0}")]
    Audit(oes_audit::AuditError),
    #[error("event dependency failed: {0}")]
    Events(oes_events::EventError),
    #[error("cluster dependency failed: {0}")]
    Cluster(String),
}

/// HTTP serving and bounded-shutdown failures.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The HTTP listener failed.
    #[error("HTTP server failed: {0}")]
    Serve(#[source] std::io::Error),
    /// In-flight requests did not finish within the configured interval.
    #[error("graceful shutdown exceeded {0:?}")]
    ShutdownTimeout(Duration),
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    #[test]
    fn incoming_request_ids_are_strictly_validated() {
        assert!(RequestId::accept("trace-123.example").is_some());
        assert!(RequestId::accept("").is_none());
        assert!(RequestId::accept("contains a space").is_none());
        assert!(RequestId::accept(&"a".repeat(129)).is_none());
    }

    #[tokio::test]
    async fn errors_use_the_stable_json_envelope() {
        let response = ApiError::not_found(RequestId("request-1".into())).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(value["error"]["code"], "ROUTE_NOT_FOUND");
        assert_eq!(value["error"]["request_id"], "request-1");
    }
}
