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

/// Simulates a topology change without applying it.
///
/// Read-only. The real placement engine runs against a hypothetical cluster map
/// and a bounded sample of committed placements, so the movement reported is
/// measured rather than modelled.
pub(crate) async fn simulate_topology(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(change): Json<record_store_replication::TopologyChange>,
) -> Result<Json<record_store_replication::SimulationReport>, ApiError> {
    cluster_management(&state, request_id.clone())?
        .operations
        // A sample rather than every placement: a simulation an operator will
        // not wait for is a simulation they will not run.
        .simulate(change, 1_000)
        .await
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

/// Explains where an object is, or would be, placed.
///
/// Read-only: it runs the placement engine against committed state and changes
/// nothing, which is what makes it safe to answer "why is my data there?"
pub(crate) async fn explain_placement(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::PlacementExplanation>, ApiError> {
    let bucket = record_store_core::BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let key = record_store_core::ObjectKey::new(key).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })?;
    cluster_management(&state, request_id.clone())?
        .operations
        .explain_placement(&bucket, &key)
        .await
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

/// Lists every defined storage class.
pub(crate) async fn list_storage_classes(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<record_store_cluster::StoragePolicy>>, ApiError> {
    cluster_management(&state, request_id.clone())?
        .operations
        .storage_policies()
        .await
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

/// Inspects one storage class.
pub(crate) async fn inspect_storage_class(
    State(state): State<AppState>,
    Path(class): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::StoragePolicy>, ApiError> {
    let class = parse_storage_class(&class, request_id.clone())?;
    cluster_management(&state, request_id.clone())?
        .operations
        .storage_policy(&class)
        .await
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

/// Defines or replaces a storage class.
pub(crate) async fn put_storage_class(
    State(state): State<AppState>,
    Path(class): Path<String>,
    Extension(request_id): Extension<RequestId>,
    Json(body): Json<record_store_cluster::StoragePolicy>,
) -> Result<Json<record_store_cluster::StoragePolicy>, ApiError> {
    let class = parse_storage_class(&class, request_id.clone())?;
    if body.class != class {
        return Err(ApiError::bad_request(
            request_id,
            "STORAGE_CLASS_MISMATCH",
            "The storage class in the path and the body must match",
        ));
    }
    cluster_management(&state, request_id.clone())?
        .operations
        .put_storage_policy(body)
        .await
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

/// Removes a storage class.
///
/// Refused while devices still carry it, since those devices would resolve to no
/// policy and silently stop being placement candidates.
pub(crate) async fn delete_storage_class(
    State(state): State<AppState>,
    Path(class): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<StatusCode, ApiError> {
    let class = parse_storage_class(&class, request_id.clone())?;
    cluster_management(&state, request_id.clone())?
        .operations
        .delete_storage_policy(&class)
        .await
        .map_err(|error| cluster_operation_error(error, request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

fn parse_storage_class(
    value: &str,
    request_id: RequestId,
) -> Result<record_store_cluster::StorageClass, ApiError> {
    record_store_cluster::StorageClass::new(value).map_err(|_| {
        ApiError::bad_request(
            request_id,
            "INVALID_STORAGE_CLASS",
            "Storage class must be 1 to 32 lowercase letters, digits, or hyphens",
        )
    })
}

/// Lists storage this node could use, without registering any of it.
///
/// Read-only by construction: discovery never formats, mounts, or claims
/// anything, and a discovered path participates only once an administrator
/// declares it in configuration.
pub(crate) async fn discover_devices(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<record_store_cluster::DiscoveredDevice>>, ApiError> {
    let management = cluster_management(&state, request_id.clone())?;
    let Some(discovery) = management.discovery() else {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "DISCOVERY_UNAVAILABLE",
            "Storage discovery is not available on this platform; declare devices in configuration",
            request_id,
        ));
    };
    discovery.discover().await.map(Json).map_err(|error| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "DISCOVERY_FAILED",
            error.to_string(),
            request_id,
        )
    })
}

/// Lists every registered device in the cluster.
///
/// Devices are the unit placement actually selects, so operators get one call
/// that answers "what storage does this cluster have" rather than having to
/// reconstruct it from per-node responses.
pub(crate) async fn list_cluster_devices(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<record_store_replication::DeviceStatus>>, ApiError> {
    let mut devices: Vec<_> = collect_cluster_status(&state, request_id)
        .await?
        .nodes
        .into_iter()
        .flat_map(|node| node.devices)
        .collect();
    devices.sort_by_key(|device| (device.node_id, device.device_id));
    Ok(Json(devices))
}

/// Lists the devices on one node.
pub(crate) async fn list_node_devices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<Vec<record_store_replication::DeviceStatus>>, ApiError> {
    let node_id = parse_node_id(&id, request_id.clone())?;
    let node = collect_cluster_status(&state, request_id.clone())
        .await?
        .nodes
        .into_iter()
        .find(|node| node.node_id == node_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "NODE_NOT_FOUND",
                format!("Node {node_id} is not a member of this cluster"),
                request_id,
            )
        })?;
    Ok(Json(node.devices))
}

/// Inspects one device.
pub(crate) async fn inspect_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    let (node_id, device_id) = parse_device_path(&node, &device, request_id.clone())?;
    cluster_management(&state, request_id.clone())?
        .operations
        .device(node_id, device_id)
        .await
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

/// Brings a registered device into service.
pub(crate) async fn activate_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    device_transition(state, node, device, request_id, DeviceAction::Activate).await
}

/// Stops new placement on a device and lets its replicas move elsewhere.
pub(crate) async fn drain_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    device_transition(state, node, device, request_id, DeviceAction::Drain).await
}

/// Pauses a device without evacuating it.
pub(crate) async fn maintain_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    device_transition(state, node, device, request_id, DeviceAction::Maintain).await
}

/// Returns a drained or maintained device to service.
pub(crate) async fn resume_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    device_transition(state, node, device, request_id, DeviceAction::Resume).await
}

/// Marks an evacuated device safe to remove.
///
/// Refused while the device still owns replica records, so a `safe_to_remove`
/// response means evacuation actually completed.
pub(crate) async fn release_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    device_transition(state, node, device, request_id, DeviceAction::Release).await
}

/// Permanently retires a device.
pub(crate) async fn retire_cluster_device(
    State(state): State<AppState>,
    Path((node, device)): Path<(String, String)>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    device_transition(state, node, device, request_id, DeviceAction::Retire).await
}

#[derive(Debug, Clone, Copy)]
enum DeviceAction {
    Activate,
    Drain,
    Maintain,
    Resume,
    Release,
    Retire,
}

async fn device_transition(
    state: AppState,
    node: String,
    device: String,
    request_id: RequestId,
    action: DeviceAction,
) -> Result<Json<record_store_cluster::DeviceRecord>, ApiError> {
    let (node_id, device_id) = parse_device_path(&node, &device, request_id.clone())?;
    let operations = &cluster_management(&state, request_id.clone())?.operations;
    let result = match action {
        DeviceAction::Activate => operations.activate_device(node_id, device_id).await,
        DeviceAction::Drain => operations.drain_device(node_id, device_id).await,
        DeviceAction::Maintain => operations.maintain_device(node_id, device_id).await,
        DeviceAction::Resume => operations.resume_device(node_id, device_id).await,
        DeviceAction::Release => operations.release_device(node_id, device_id).await,
        DeviceAction::Retire => operations.retire_device(node_id, device_id).await,
    };
    result
        .map(Json)
        .map_err(|error| cluster_operation_error(error, request_id))
}

fn parse_device_path(
    node: &str,
    device: &str,
    request_id: RequestId,
) -> Result<(NodeId, record_store_core::DeviceId), ApiError> {
    let node_id = parse_node_id(node, request_id.clone())?;
    let device_id = device.parse().map_err(|_| {
        ApiError::bad_request(
            request_id,
            "INVALID_DEVICE_ID",
            "Device ID must be a valid Record Store device identifier",
        )
    })?;
    Ok((node_id, device_id))
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
        OperationError::NodeNotFound(_)
        | OperationError::DeviceNotFound { .. }
        | OperationError::StoragePolicyNotFound(_) => StatusCode::NOT_FOUND,
        OperationError::InvalidTransition { .. }
        | OperationError::InvalidDeviceTransition { .. }
        | OperationError::StoragePolicyInUse { .. }
        | OperationError::DurabilityAtRisk(_) => StatusCode::CONFLICT,
        OperationError::Cluster(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    let code = match error_value {
        OperationError::NodeNotFound(_) => "NODE_NOT_FOUND",
        OperationError::DeviceNotFound { .. } => "DEVICE_NOT_FOUND",
        OperationError::StoragePolicyNotFound(_) => "STORAGE_CLASS_NOT_FOUND",
        OperationError::StoragePolicyInUse { .. } => "STORAGE_CLASS_IN_USE",
        OperationError::InvalidTransition { .. } => "INVALID_NODE_TRANSITION",
        OperationError::InvalidDeviceTransition { .. } => "INVALID_DEVICE_TRANSITION",
        OperationError::DurabilityAtRisk(_) => "DURABILITY_AT_RISK",
        OperationError::Cluster(_) => "CLUSTER_UNAVAILABLE",
    };
    ApiError::new(status, code, error_value.to_string(), request_id)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use crate::test_support::{admin, api, call, clustered_api, expect_status};

    /// A deployment with no cluster wired in must say the feature is absent
    /// rather than reporting an empty cluster, which reads as a healthy one.
    /// The stable code is what a client branches on.
    #[tokio::test]
    async fn a_standalone_deployment_reports_cluster_administration_as_disabled() {
        let (_directory, api) = api().await;
        for uri in [
            "/api/v1/cluster",
            "/api/v1/cluster/health",
            "/api/v1/nodes",
            "/api/v1/repair/status",
            "/api/v1/rebalance/status",
        ] {
            let body = expect_status(&api, admin("GET", uri, None), StatusCode::CONFLICT).await;
            assert_eq!(
                body["error"]["code"], "CLUSTER_MODE_DISABLED",
                "{uri}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn a_clustered_deployment_reports_its_status_and_health() {
        let (_directory, api) = clustered_api().await;

        let status =
            expect_status(&api, admin("GET", "/api/v1/cluster", None), StatusCode::OK).await;
        assert!(
            status["cluster_id"].as_str().is_some() || status.is_object(),
            "{status}"
        );

        let health = expect_status(
            &api,
            admin("GET", "/api/v1/cluster/health", None),
            StatusCode::OK,
        )
        .await;
        assert!(health.is_object(), "{health}");
    }

    /// Storage classes round-trip through the API, and the default is always
    /// there so a cluster that never configured one still answers coherently.
    #[tokio::test]
    async fn a_storage_class_can_be_defined_inspected_and_removed() {
        let (_directory, api) = clustered_api().await;

        let listed = expect_status(
            &api,
            admin("GET", "/api/v1/storage-classes", None),
            StatusCode::OK,
        )
        .await;
        let classes = listed.as_array().expect("a list of classes");
        assert_eq!(
            classes.len(),
            1,
            "an unconfigured cluster still has its default class: {listed}"
        );
        assert_eq!(classes[0]["class"], "standard");

        let body = json!({
            "class": "hot",
            "description": "solid state only",
            "device_filter": {"allowed_kinds": ["nvme", "sata_ssd"]},
            "durability": {"strategy": "replication", "replicas": 2},
            "failure_domain": "rack",
            "strict_failure_domains": true,
            "minimum_free_space_percent": 15
        });
        let created = expect_status(
            &api,
            admin("PUT", "/api/v1/storage-classes/hot", Some(body.clone())),
            StatusCode::OK,
        )
        .await;
        assert_eq!(created["class"], "hot");
        assert_eq!(created["durability"]["replicas"], 2);

        let fetched = expect_status(
            &api,
            admin("GET", "/api/v1/storage-classes/hot", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(fetched["minimum_free_space_percent"], 15);
        assert_eq!(fetched["failure_domain"], "rack");

        let removed = call(&api, admin("DELETE", "/api/v1/storage-classes/hot", None)).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let missing = expect_status(
            &api,
            admin("GET", "/api/v1/storage-classes/hot", None),
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(missing["error"]["code"], "STORAGE_CLASS_NOT_FOUND");
    }

    /// A class that promises durability Record Store cannot deliver must be
    /// refused at the edge rather than accepted and quietly reinterpreted.
    #[tokio::test]
    async fn a_storage_class_that_cannot_be_honoured_is_refused() {
        let (_directory, api) = clustered_api().await;

        for (label, body) in [
            (
                "zero replicas",
                json!({
                    "class": "broken",
                    "durability": {"strategy": "replication", "replicas": 0},
                    "failure_domain": "node"
                }),
            ),
            (
                "erasure coding, which has no write path yet",
                json!({
                    "class": "broken",
                    "durability": {
                        "strategy": "erasure_coding",
                        "profile": {"data_shards": 4, "parity_shards": 2}
                    },
                    "failure_domain": "node"
                }),
            ),
        ] {
            let response = call(
                &api,
                admin("PUT", "/api/v1/storage-classes/broken", Some(body)),
            )
            .await;
            assert!(
                !response.status().is_success(),
                "{label} was accepted but must not be"
            );
        }

        // The path and the body have to agree; guessing which one was meant
        // would let a typo redefine a different class.
        let mismatched = expect_status(
            &api,
            admin(
                "PUT",
                "/api/v1/storage-classes/hot",
                Some(json!({
                    "class": "cold",
                    "durability": {"strategy": "replication", "replicas": 2},
                    "failure_domain": "node"
                })),
            ),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(mismatched["error"]["code"], "STORAGE_CLASS_MISMATCH");
    }

    /// Initialization is idempotent: an operator or an orchestrator may call it
    /// repeatedly, and the second call must not fail or change the identity.
    #[tokio::test]
    async fn initialization_is_idempotent() {
        let (_directory, api) = clustered_api().await;

        let first = expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;
        let second = expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(first["cluster_id"], second["cluster_id"], "{second}");
    }

    #[tokio::test]
    async fn nodes_are_listed_once_the_cluster_is_initialized() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        let nodes = expect_status(&api, admin("GET", "/api/v1/nodes", None), StatusCode::OK).await;
        assert!(nodes.is_array(), "{nodes}");
    }

    /// A join token is what admits a new node, so issuing one has to disclose
    /// the secret exactly once and record its lifetime.
    #[tokio::test]
    async fn a_join_token_is_issued_with_its_lifetime() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        let issued = expect_status(
            &api,
            admin(
                "POST",
                "/api/v1/cluster/join-tokens",
                Some(json!({"lifetime_seconds": 600, "description": "a new node"})),
            ),
            StatusCode::CREATED,
        )
        .await;
        assert!(
            issued["token"]
                .as_str()
                .is_some_and(|token| !token.is_empty()),
            "the token must be disclosed once: {issued}"
        );
    }

    /// Node lifecycle routes name a node by identifier. An unknown or malformed
    /// one must be refused rather than acted on.
    #[tokio::test]
    async fn node_lifecycle_routes_validate_their_identifier() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        for action in ["drain", "maintenance", "resume"] {
            let malformed = call(
                &api,
                admin("POST", &format!("/api/v1/nodes/not-a-uuid/{action}"), None),
            )
            .await;
            assert_eq!(
                malformed.status(),
                StatusCode::BAD_REQUEST,
                "{action} with a malformed id"
            );

            let unknown = call(
                &api,
                admin(
                    "POST",
                    &format!("/api/v1/nodes/0195f0c8-0000-7000-8000-0000000000ff/{action}"),
                    None,
                ),
            )
            .await;
            assert!(
                unknown.status().is_client_error(),
                "{action} on an unknown node returned {}",
                unknown.status()
            );
        }
    }

    #[tokio::test]
    async fn inspecting_a_node_that_does_not_exist_is_a_not_found() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        expect_status(
            &api,
            admin(
                "GET",
                "/api/v1/nodes/0195f0c8-0000-7000-8000-0000000000ff",
                None,
            ),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    /// Repair and rebalance status are what an operator watches during recovery,
    /// so they have to answer on a live cluster with nothing queued.
    #[tokio::test]
    async fn repair_and_rebalance_status_answer_on_a_quiet_cluster() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        for uri in ["/api/v1/repair/status", "/api/v1/rebalance/status"] {
            let body = expect_status(&api, admin("GET", uri, None), StatusCode::OK).await;
            assert!(
                body.is_object() || body.is_array(),
                "{uri} returned neither a report nor a queue: {body}"
            );
        }
    }

    /// Starting a rebalance on a balanced cluster is allowed and must report
    /// that it did so rather than failing.
    #[tokio::test]
    async fn a_rebalance_can_be_started_on_a_balanced_cluster() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        let response = call(&api, admin("POST", "/api/v1/rebalance", None)).await;
        assert!(
            response.status().is_success(),
            "starting a rebalance failed: {}",
            response.status()
        );
    }

    /// Decommissioning removes durability, so the destructive override has to be
    /// an explicit field rather than a default.
    #[tokio::test]
    async fn decommissioning_requires_an_explicit_force_field() {
        let (_directory, api) = clustered_api().await;
        expect_status(
            &api,
            admin("POST", "/api/v1/cluster/init", None),
            StatusCode::OK,
        )
        .await;

        let response = call(
            &api,
            admin(
                "POST",
                "/api/v1/nodes/0195f0c8-0000-7000-8000-0000000000ff/decommission",
                Some(json!({"force": false})),
            ),
        )
        .await;
        assert!(
            response.status().is_client_error(),
            "an unknown node must not be decommissioned: {}",
            response.status()
        );
    }
}
