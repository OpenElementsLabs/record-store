//! Native operational and authenticated management HTTP API.

mod sharing;

use std::{
    future::{Future, IntoFuture},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::auth::{ManagementPrincipal, auth_session};
use crate::error::{ApiError, ReadinessError};
use crate::handlers::accounts::{
    create_service_account, delete_service_account, get_service_account,
    issue_temporary_credential, list_service_accounts, rotate_credential, set_credential_status,
    set_service_account_status,
};
use crate::handlers::audit::list_audit_events;
use crate::handlers::buckets::{
    create_bucket, delete_bucket, get_bucket_versioning, list_buckets, set_bucket_quota,
    set_bucket_versioning,
};
use crate::handlers::cluster::{
    cluster_health, cluster_initialize, cluster_status, decommission_cluster_node,
    drain_cluster_node, inspect_cluster_node, issue_cluster_join_token, list_cluster_nodes,
    maintain_cluster_node, rebalance_status, repair_status, resume_cluster_node, start_rebalance,
};
use crate::handlers::lifecycle::{
    create_lifecycle_rule, delete_lifecycle_rule, list_lifecycle_rules, update_lifecycle_rule,
};
use crate::handlers::maintenance::{copy_object, restore_version, verify_bucket, verify_object};
use crate::handlers::objects::{
    delete_bucket_object, delete_bucket_object_version, download_bucket_object, get_bucket_object,
    list_bucket_object_versions, list_bucket_objects, list_storage_events, preview_bucket_object,
    upload_bucket_object,
};
use crate::handlers::policies::{attach_policy, create_policy, detach_policy, list_policies};
use crate::handlers::storage::{storage_inspect, storage_repair, storage_status, storage_usage};
use crate::handlers::system::{health, ready, system_info};
use crate::handlers::webhooks::{
    create_webhook, delete_webhook, list_webhook_deliveries, list_webhooks, set_webhook_status,
};
use crate::metrics::{metrics, system_metrics};
use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, Extension, Request, State},
    http::{HeaderValue, StatusCode, header::HeaderName},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use record_store_audit::{AuditEvent, AuditRepository, AuditResult};
use record_store_auth::CredentialManager;
use record_store_config::DeploymentMode;
use record_store_consensus::MetadataConsensus;
use record_store_core::OrganizationId;
use record_store_events::EventRepository;
use record_store_metadata::MetadataRepository;
use record_store_replication::{ClusterContext, ClusterOperations, ClusterStatus, TaskHealth};
use record_store_service::Services;
use record_store_sharing::redact_capability_path;
use record_store_storage::ObjectStore;
use tokio::{net::TcpListener, time::timeout};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, error, info, info_span};

pub use crate::sharing::SharingManagement;

static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Bounded counters for preview and capability traffic.
///
/// Every value is a plain total with no labels at all. That is deliberate: the
/// natural dimensions here — a token, an object key, a request identifier —
/// are unbounded and attacker-influenced, and putting any of them on a metric
/// would turn a scrape endpoint into a memory leak with a public trigger.
#[derive(Debug, Default)]
pub struct SharingMetrics {
    preview_requests: std::sync::atomic::AtomicU64,
    preview_failures: std::sync::atomic::AtomicU64,
    share_accesses: std::sync::atomic::AtomicU64,
    share_denials: std::sync::atomic::AtomicU64,
    shares_created: std::sync::atomic::AtomicU64,
    embed_requests: std::sync::atomic::AtomicU64,
    embed_denials: std::sync::atomic::AtomicU64,
    embeds_created: std::sync::atomic::AtomicU64,
}

impl SharingMetrics {
    fn bump(counter: &std::sync::atomic::AtomicU64) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn read(counter: &std::sync::atomic::AtomicU64) -> u64 {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn preview_request(&self) {
        Self::bump(&self.preview_requests);
    }

    fn preview_failure(&self) {
        Self::bump(&self.preview_failures);
    }

    fn share_access(&self) {
        Self::bump(&self.share_accesses);
    }

    fn share_denied(&self) {
        Self::bump(&self.share_denials);
    }

    fn shares_created(&self) {
        Self::bump(&self.shares_created);
    }

    fn embed_request(&self) {
        Self::bump(&self.embed_requests);
    }

    fn embed_denied(&self) {
        Self::bump(&self.embed_denials);
    }

    fn embeds_created(&self) {
        Self::bump(&self.embeds_created);
    }
}

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
    sharing: Option<SharingManagement>,
    sharing_metrics: Arc<SharingMetrics>,
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
            sharing: None,
            sharing_metrics: Arc::new(SharingMetrics::default()),
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

    /// Adds share and embed capabilities, including their public routes.
    ///
    /// Absent by default. A deployment that never calls this serves no public
    /// capability surface at all, and the management routes report the feature
    /// as unavailable rather than failing in some less obvious way.
    #[must_use]
    pub fn with_sharing(mut self, sharing: SharingManagement) -> Self {
        self.sharing = Some(sharing);
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
        if let Some(sharing) = &self.sharing {
            sharing
                .service()
                .store()
                .check_ready()
                .await
                .map_err(|error| ReadinessError::Sharing(error.to_string()))?;
        }
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

mod auth;
mod dto;
mod error;
mod handlers;
mod metrics;

pub use auth::{ManagementAuth, ManagementRole, MetricsAuth};
pub use dto::RequestId;
pub use error::ServerError;

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
            "/api/v1/buckets/{bucket}/object-preview/{*key}",
            get(preview_bucket_object),
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
        .route("/api/v1/sharing/settings", get(sharing::sharing_settings))
        .route(
            "/api/v1/buckets/{bucket}/object-shares/{*key}",
            get(sharing::list_object_shares).post(sharing::create_object_share),
        )
        .route(
            "/api/v1/shares/{id}",
            get(sharing::get_share).delete(sharing::delete_share),
        )
        .route("/api/v1/shares/{id}/url", get(sharing::get_share_url))
        .route(
            "/api/v1/shares/{id}/revoke",
            axum::routing::post(sharing::revoke_share),
        )
        .route(
            "/api/v1/buckets/{bucket}/object-embeds/{*key}",
            get(sharing::list_object_embeds).post(sharing::create_object_embed),
        )
        .route(
            "/api/v1/embeds/{id}",
            get(sharing::get_embed)
                .patch(sharing::update_embed)
                .delete(sharing::delete_embed),
        )
        .route("/api/v1/embeds/{id}/url", get(sharing::get_embed_url))
        .route(
            "/api/v1/embeds/{id}/revoke",
            axum::routing::post(sharing::revoke_embed),
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

    // Public share delivery. These routes carry no session and are deliberately
    // outside the administrative tree: the token in the path is the entire
    // authorization, and it is re-checked against durable state on every request
    // so a revocation lands on the next one. Embed delivery is not here — it
    // belongs on the storage data plane, and [`embed_router`] mounts it there.
    let public_capabilities = Router::new()
        .route("/s/{token}", get(sharing::public_share_descriptor))
        .route(
            "/s/{token}/unlock",
            axum::routing::post(sharing::public_share_unlock),
        )
        .route("/s/{token}/content", get(sharing::public_share_content));

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .merge(operational_metrics)
        .merge(public_capabilities)
        .merge(administrative)
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_context,
        ))
        .with_state(state)
}

/// Public embed delivery, mounted on the storage data plane rather than here.
///
/// An embed URL is pasted into somebody else's `<img>` tag, so it has to live
/// where object bytes already live: the S3-compatible endpoint a deployment
/// publishes, not the administrative console. Two things follow from that. A
/// site loading an asset never touches the management plane, which can stay
/// closed to the internet; and the bytes never travel through a document server
/// that would have to re-issue them through a body limit.
///
/// The router carries no authentication layer at all. That is the point: the
/// opaque token in the path is the entire authorization, and it is re-resolved
/// against durable state on every request. It is merged alongside the S3
/// operations rather than inside them, so nothing here can reach an S3 handler
/// and no S3 credential is ever consulted. The prefix cannot collide with a
/// bucket, because a bucket name is at least three characters.
pub fn embed_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/e/{token}",
            get(sharing::public_embed_content).options(sharing::public_embed_preflight),
        )
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
    // A public capability route carries its secret in the path, and the tracing
    // stack records paths verbatim. Redacting here — before the span is opened
    // and before anything else in this function sees it — is what keeps the
    // ordinary request log from becoming a list of working share links.
    let logged_path = redact_capability_path(&path);
    // Resolved once, here, because every public capability handler needs it and
    // because deciding what counts as "one client" is a policy question that
    // deserves a single answer rather than one per route.
    let client = ClientIdentity(sharing::client_identity(
        request.headers(),
        request.extensions().get::<ConnectInfo<SocketAddr>>(),
    ));
    request.extensions_mut().insert(client);
    let source_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect| connect.0.ip().to_string());
    let started = Instant::now();
    let span = info_span!(
        "http.request",
        request_id = %request_id,
        method = %method,
        route = %logged_path
    );
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
            event_id: record_store_core::AuditEventId::new(),
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
            operation: format!("{method} {logged_path}"),
            resource: logged_path,
            result,
            metadata: Default::default(),
        };
        if let Err(error) = state.audit.append(&event).await {
            error!(%error, request_id = %request_id, "durable audit append failed");
        }
    }
    response
}

/// Who abuse controls treat one public request as coming from.
///
/// Resolved once per request from the forwarded address or the socket, and
/// carried as a value so that no handler is tempted to invent its own rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientIdentity(String);

impl ClientIdentity {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
