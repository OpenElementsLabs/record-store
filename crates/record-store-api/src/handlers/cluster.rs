use axum::{
    Json,
    extract::{Extension, Path, State},
    http::StatusCode,
};
use record_store_cluster::ClusterOperationKind;
use record_store_core::NodeId;
use record_store_replication::{ClusterStatus, OperationError};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::error::ApiError;
use crate::*;

pub(crate) async fn cluster_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ClusterStatus>, ApiError> {
    Ok(Json(collect_cluster_status(&state, request_id).await?))
}

pub(crate) async fn cluster_initialize(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ClusterStatus>, ApiError> {
    // Cluster-mode servers form and persist their initial one-member consensus
    // group before accepting HTTP traffic. Keeping this endpoint idempotent
    // gives operators one stable `record-store cluster init` workflow without allowing a
    // second cluster identity to be created accidentally.
    Ok(Json(collect_cluster_status(&state, request_id).await?))
}

#[derive(Serialize)]
pub(crate) struct ClusterHealthResponse {
    health: record_store_cluster::ClusterHealth,
    reasons: Vec<String>,
    metadata: record_store_consensus::MetadataQuorum,
    data: record_store_cluster::DataHealth,
}

pub(crate) async fn cluster_health(
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

pub(crate) async fn list_cluster_nodes(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<record_store_replication::NodeStatus>>, ApiError> {
    Ok(Json(
        collect_cluster_status(&state, request_id).await?.nodes,
    ))
}

pub(crate) async fn inspect_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_replication::NodeStatus>, ApiError> {
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

pub(crate) async fn drain_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::ClusterOperation>, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    let operation = cluster_management(&state, request_id.clone())?
        .operations
        .drain(node_id)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(Json(operation))
}

pub(crate) async fn maintain_cluster_node(
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

pub(crate) async fn resume_cluster_node(
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
pub(crate) struct DecommissionInput {
    #[serde(default)]
    force: bool,
}

pub(crate) async fn decommission_cluster_node(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
    input: Option<Json<DecommissionInput>>,
) -> Result<Json<record_store_cluster::ClusterOperation>, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    let force = input.map(|Json(input)| input.force).unwrap_or_default();
    let operation = cluster_management(&state, request_id.clone())?
        .operations
        .decommission(node_id, force)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(Json(operation))
}

pub(crate) async fn repair_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_replication::RepairStatus>, ApiError> {
    Ok(Json(
        collect_cluster_status(&state, request_id).await?.repair,
    ))
}

pub(crate) async fn start_rebalance(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::ClusterOperation>, ApiError> {
    let operation = cluster_management(&state, request_id.clone())?
        .operations
        .rebalance()
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(Json(operation))
}

pub(crate) async fn rebalance_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<record_store_cluster::ClusterOperation>>, ApiError> {
    let operations = collect_cluster_status(&state, request_id)
        .await?
        .operations
        .into_iter()
        .filter(|operation| operation.kind == ClusterOperationKind::Rebalance)
        .collect();
    Ok(Json(operations))
}

#[derive(Deserialize)]
pub(crate) struct JoinTokenInput {
    #[serde(default = "default_join_token_lifetime")]
    lifetime_seconds: u64,
    #[serde(default)]
    description: String,
}

pub(crate) const fn default_join_token_lifetime() -> u64 {
    3_600
}

#[derive(Serialize)]
pub(crate) struct IssuedJoinTokenResponse {
    id: record_store_core::JoinTokenId,
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) async fn issue_cluster_join_token(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(input): Json<JoinTokenInput>,
) -> Result<(StatusCode, Json<IssuedJoinTokenResponse>), ApiError> {
    if !(record_store_cluster::JoinToken::MINIMUM_LIFETIME_SECONDS
        ..=record_store_cluster::JoinToken::MAXIMUM_LIFETIME_SECONDS)
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

pub(crate) fn cluster_management(
    state: &AppState,
    request_id: RequestId,
) -> Result<&ClusterManagement, ApiError> {
    state.cluster.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "CLUSTER_MODE_DISABLED",
            "This Record Store process is running in standalone mode",
            request_id,
        )
    })
}

pub(crate) async fn collect_cluster_status(
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

pub(crate) fn parse_node_id(value: &str, request_id: RequestId) -> Result<NodeId, ApiError> {
    value.parse().map_err(|_| {
        ApiError::bad_request(
            request_id,
            "INVALID_NODE_ID",
            "Node ID must be a valid Record Store node identifier",
        )
    })
}

pub(crate) fn cluster_operation_error(
    error_value: OperationError,
    request_id: RequestId,
) -> ApiError {
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
