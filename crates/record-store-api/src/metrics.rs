use axum::{
    Json,
    extract::{Extension, State},
    http::header,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tracing::error;

use crate::error::{ApiError, internal_service_error};
use crate::*;

/// Everything both metric representations are built from.
///
/// Gathering once and rendering twice is what keeps the Prometheus exposition
/// and the console's JSON view from drifting apart. Prometheus scrapes with a
/// dedicated credential; the console reads the same numbers with a management
/// token, because it must never hold the scrape token.
#[derive(Debug, Serialize)]
pub(crate) struct MetricsSnapshot {
    /// Requests served since this process started.
    pub(crate) requests: u64,
    /// Requests that failed since this process started.
    pub(crate) errors: u64,
    /// Bytes accepted from clients since this process started.
    pub(crate) upload_bytes: u64,
    /// Bytes served to clients since this process started.
    pub(crate) download_bytes: u64,
    pub(crate) storage: StorageMetrics,
    /// Preview, share, and embed activity.
    pub(crate) sharing: CapabilityMetrics,
    /// Present only in cluster mode, so a standalone console shows no cluster
    /// figures rather than zeroes that look like a broken cluster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cluster: Option<ClusterMetrics>,
}

/// Preview and capability counters, plus live capability counts.
#[derive(Debug, Default, Serialize)]
pub(crate) struct CapabilityMetrics {
    pub(crate) preview_requests: u64,
    pub(crate) preview_failures: u64,
    pub(crate) shares_created: u64,
    pub(crate) share_access: u64,
    pub(crate) share_access_denied: u64,
    pub(crate) share_links_active: u64,
    pub(crate) embeds_created: u64,
    pub(crate) embed_requests: u64,
    pub(crate) embed_denied: u64,
    pub(crate) embeds_active: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct StorageMetrics {
    pub(crate) object_count: u64,
    pub(crate) bucket_count: u64,
    pub(crate) version_count: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) physical_bytes: u64,
    pub(crate) multipart_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClusterMetrics {
    pub(crate) nodes: u64,
    pub(crate) healthy: bool,
    pub(crate) quorum_writable: bool,
    pub(crate) under_replicated_objects: u64,
    /// Repair tasks currently running. Exposed to Prometheus under the older
    /// name `record_store_replication_queue_depth`, which is kept for existing scrapers.
    pub(crate) repair_active_tasks: u64,
    pub(crate) node_capacity_bytes: u64,
    pub(crate) node_used_bytes: u64,
    pub(crate) node_available_bytes: u64,
    pub(crate) logical_bytes: u64,
    pub(crate) physical_bytes: u64,
}

/// Collects the current metric values.
pub(crate) async fn gather_metrics(
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
                    healthy: status.health == record_store_cluster::ClusterHealth::Healthy,
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

    let counters = &state.sharing_metrics;
    let mut sharing_metrics = CapabilityMetrics {
        preview_requests: SharingMetrics::read(&counters.preview_requests),
        preview_failures: SharingMetrics::read(&counters.preview_failures),
        shares_created: SharingMetrics::read(&counters.shares_created),
        share_access: SharingMetrics::read(&counters.share_accesses),
        share_access_denied: SharingMetrics::read(&counters.share_denials),
        embeds_created: SharingMetrics::read(&counters.embeds_created),
        embed_requests: SharingMetrics::read(&counters.embed_requests),
        embed_denied: SharingMetrics::read(&counters.embed_denials),
        ..CapabilityMetrics::default()
    };
    if let Some(sharing) = &state.sharing {
        let now = chrono::Utc::now();
        match sharing.service().store().list_shares().await {
            Ok(links) => {
                sharing_metrics.share_links_active = links
                    .iter()
                    .filter(|link| link.status(now).usable())
                    .count() as u64;
            }
            // A capability read failure must not fail the whole scrape.
            Err(error) => error!(%error, "active share count could not be collected"),
        }
        match sharing.service().store().list_embeds().await {
            Ok(links) => {
                sharing_metrics.embeds_active = links
                    .iter()
                    .filter(|link| link.status(now).usable())
                    .count() as u64;
            }
            Err(error) => error!(%error, "active embed count could not be collected"),
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
        sharing: sharing_metrics,
        cluster: cluster_metrics,
    })
}

/// Renders one snapshot as Prometheus text exposition.
pub(crate) fn prometheus_exposition(snapshot: &MetricsSnapshot) -> String {
    let mut body = String::new();
    let mut gauge = |name: &str, kind: &str, value: u64| {
        body.push_str(&format!("# TYPE {name} {kind}\n{name} {value}\n"));
    };
    gauge(
        "record_store_s3_requests_total",
        "counter",
        snapshot.requests,
    );
    gauge("record_store_requests_total", "counter", snapshot.requests);
    gauge("record_store_errors_total", "counter", snapshot.errors);
    gauge(
        "record_store_objects_total",
        "gauge",
        snapshot.storage.object_count,
    );
    gauge(
        "record_store_storage_bytes",
        "gauge",
        snapshot.storage.logical_bytes,
    );
    gauge(
        "record_store_versions_total",
        "gauge",
        snapshot.storage.version_count,
    );
    gauge(
        "record_store_buckets_total",
        "gauge",
        snapshot.storage.bucket_count,
    );
    gauge(
        "record_store_storage_logical_bytes",
        "gauge",
        snapshot.storage.logical_bytes,
    );
    gauge(
        "record_store_storage_physical_bytes",
        "gauge",
        snapshot.storage.physical_bytes,
    );
    gauge(
        "record_store_multipart_bytes",
        "gauge",
        snapshot.storage.multipart_bytes,
    );
    gauge(
        "record_store_preview_requests_total",
        "counter",
        snapshot.sharing.preview_requests,
    );
    gauge(
        "record_store_preview_failures_total",
        "counter",
        snapshot.sharing.preview_failures,
    );
    gauge(
        "record_store_share_links_created_total",
        "counter",
        snapshot.sharing.shares_created,
    );
    gauge(
        "record_store_share_access_total",
        "counter",
        snapshot.sharing.share_access,
    );
    gauge(
        "record_store_share_access_denied_total",
        "counter",
        snapshot.sharing.share_access_denied,
    );
    gauge(
        "record_store_share_links_active",
        "gauge",
        snapshot.sharing.share_links_active,
    );
    gauge(
        "record_store_embeds_created_total",
        "counter",
        snapshot.sharing.embeds_created,
    );
    gauge(
        "record_store_embed_requests_total",
        "counter",
        snapshot.sharing.embed_requests,
    );
    gauge(
        "record_store_embed_denied_total",
        "counter",
        snapshot.sharing.embed_denied,
    );
    gauge(
        "record_store_embeds_active",
        "gauge",
        snapshot.sharing.embeds_active,
    );
    gauge(
        "record_store_upload_bytes_total",
        "counter",
        snapshot.upload_bytes,
    );
    gauge(
        "record_store_download_bytes_total",
        "counter",
        snapshot.download_bytes,
    );
    if let Some(cluster) = &snapshot.cluster {
        gauge(
            "record_store_node_capacity_bytes",
            "gauge",
            cluster.node_capacity_bytes,
        );
        gauge(
            "record_store_node_used_bytes",
            "gauge",
            cluster.node_used_bytes,
        );
        gauge(
            "record_store_node_available_bytes",
            "gauge",
            cluster.node_available_bytes,
        );
        gauge(
            "record_store_node_health",
            "gauge",
            u64::from(cluster.healthy),
        );
        gauge("record_store_cluster_nodes", "gauge", cluster.nodes);
        gauge(
            "record_store_under_replicated_objects",
            "gauge",
            cluster.under_replicated_objects,
        );
        gauge(
            "record_store_replication_queue_depth",
            "gauge",
            cluster.repair_active_tasks,
        );
        gauge(
            "record_store_metadata_quorum_health",
            "gauge",
            u64::from(cluster.quorum_writable),
        );
        gauge(
            "record_store_cluster_logical_bytes",
            "gauge",
            cluster.logical_bytes,
        );
        gauge(
            "record_store_cluster_physical_bytes",
            "gauge",
            cluster.physical_bytes,
        );
    }
    body
}

pub(crate) async fn metrics(
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
pub(crate) async fn system_metrics(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<MetricsSnapshot>, ApiError> {
    gather_metrics(&state, &request_id).await.map(Json)
}
