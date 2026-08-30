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
    /// Devices registered cluster-wide.
    pub(crate) devices: u64,
    /// Devices currently eligible for new placement.
    pub(crate) devices_accepting_placement: u64,
    /// Devices being evacuated.
    pub(crate) devices_draining: u64,
    /// Devices whose data no longer counts toward durability.
    pub(crate) devices_failed: u64,
    /// Devices held out of service by an administrator.
    pub(crate) devices_unavailable: u64,
    /// Raw capacity exposed by every registered device.
    pub(crate) device_capacity_raw_bytes: u64,
    /// Capacity Record Store may allocate from.
    pub(crate) device_capacity_usable_bytes: u64,
    /// Capacity currently free.
    pub(crate) device_capacity_available_bytes: u64,
}

/// Device counts and capacity, aggregated so cardinality stays bounded.
#[derive(Default)]
struct DeviceInventory {
    total: u64,
    accepting: u64,
    draining: u64,
    failed: u64,
    unavailable: u64,
    raw_bytes: u64,
    usable_bytes: u64,
    available_bytes: u64,
}

impl DeviceInventory {
    fn observe(&mut self, device: &record_store_replication::DeviceStatus) {
        use record_store_cluster::{DeviceHealth, DeviceState};

        self.total += 1;
        if device.accepts_placement {
            self.accepting += 1;
        }
        if device.state == DeviceState::Draining {
            self.draining += 1;
        }
        // A device is failed if either the administrator or the platform says
        // so; the two are recorded separately and either one is disqualifying.
        if device.state == DeviceState::Failed || device.health == DeviceHealth::Failed {
            self.failed += 1;
        }
        if matches!(
            device.state,
            DeviceState::Maintenance | DeviceState::SafeToRemove | DeviceState::Retired
        ) {
            self.unavailable += 1;
        }
        self.raw_bytes = self.raw_bytes.saturating_add(device.capacity_bytes);
        self.usable_bytes = self.usable_bytes.saturating_add(device.usable_bytes);
        self.available_bytes = self.available_bytes.saturating_add(device.available_bytes);
    }
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
                // Counted by state rather than emitted per device: a series per
                // device would grow without bound as hardware is replaced, and
                // Prometheus cardinality is a resource like any other.
                let devices = status.nodes.iter().flat_map(|node| &node.devices);
                let mut inventory = DeviceInventory::default();
                for device in devices {
                    inventory.observe(device);
                }
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
                    devices: inventory.total,
                    devices_accepting_placement: inventory.accepting,
                    devices_draining: inventory.draining,
                    devices_failed: inventory.failed,
                    devices_unavailable: inventory.unavailable,
                    device_capacity_raw_bytes: inventory.raw_bytes,
                    device_capacity_usable_bytes: inventory.usable_bytes,
                    device_capacity_available_bytes: inventory.available_bytes,
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
        gauge("record_store_devices_total", "gauge", cluster.devices);
        gauge(
            "record_store_devices_accepting_placement",
            "gauge",
            cluster.devices_accepting_placement,
        );
        gauge(
            "record_store_devices_draining",
            "gauge",
            cluster.devices_draining,
        );
        gauge(
            "record_store_devices_failed",
            "gauge",
            cluster.devices_failed,
        );
        gauge(
            "record_store_devices_unavailable",
            "gauge",
            cluster.devices_unavailable,
        );
        gauge(
            "record_store_device_capacity_raw_bytes",
            "gauge",
            cluster.device_capacity_raw_bytes,
        );
        gauge(
            "record_store_device_capacity_usable_bytes",
            "gauge",
            cluster.device_capacity_usable_bytes,
        );
        gauge(
            "record_store_device_capacity_available_bytes",
            "gauge",
            cluster.device_capacity_available_bytes,
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

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;

    use super::*;
    use crate::test_support::{
        METRICS_TOKEN, admin, api, call, expect_status, make_bucket, put_object, signed,
    };

    async fn text(response: axum::response::Response<Body>) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("UTF-8")
    }

    /// Device counts are derived from state and health, and a device is only
    /// eligible when both agree.
    #[test]
    fn device_counts_follow_lifecycle_and_health_independently() {
        use record_store_cluster::{DeviceHealth, DeviceKind, DeviceState};
        use record_store_core::{DeviceId, NodeId};

        fn device(
            state: DeviceState,
            health: DeviceHealth,
        ) -> record_store_replication::DeviceStatus {
            record_store_replication::DeviceStatus {
                device_id: DeviceId::new(),
                node_id: NodeId::new(),
                kind: DeviceKind::FilesystemDirectory,
                storage_class: "standard".into(),
                state,
                health,
                capacity_bytes: 1_000,
                usable_bytes: 900,
                available_bytes: 400,
                utilization_percent: 55,
                configured_weight: 1_000,
                accepts_placement: state.accepts_new_placements() && health.permits_placement(),
                current_path: None,
                model: None,
            }
        }

        let mut inventory = DeviceInventory::default();
        for entry in [
            device(DeviceState::Active, DeviceHealth::Healthy),
            device(DeviceState::Active, DeviceHealth::Unknown),
            // Administratively fine, but the platform reports it dead.
            device(DeviceState::Active, DeviceHealth::Failed),
            device(DeviceState::Draining, DeviceHealth::Healthy),
            device(DeviceState::Failed, DeviceHealth::Unknown),
            device(DeviceState::Maintenance, DeviceHealth::Healthy),
            device(DeviceState::Retired, DeviceHealth::Unknown),
        ] {
            inventory.observe(&entry);
        }

        assert_eq!(inventory.total, 7);
        assert_eq!(
            inventory.accepting, 2,
            "only active devices with placement-permitting health may take new data"
        );
        assert_eq!(inventory.draining, 1);
        assert_eq!(
            inventory.failed, 2,
            "a device is failed if either the administrator or the platform says so"
        );
        assert_eq!(
            inventory.unavailable, 2,
            "maintenance and retired are held out of service"
        );
        assert_eq!(inventory.raw_bytes, 7_000);
        assert_eq!(inventory.usable_bytes, 6_300);
        assert_eq!(inventory.available_bytes, 2_800);
    }

    /// Device metrics carry no labels.
    ///
    /// A series per device would grow without bound as hardware is replaced, so
    /// the scrape reports counts by state and summed capacity instead.
    #[test]
    fn device_metrics_are_aggregated_and_carry_no_identifiers() {
        let snapshot = MetricsSnapshot {
            requests: 0,
            errors: 0,
            upload_bytes: 0,
            download_bytes: 0,
            storage: StorageMetrics {
                object_count: 0,
                bucket_count: 0,
                version_count: 0,
                logical_bytes: 0,
                physical_bytes: 0,
                multipart_bytes: 0,
            },
            sharing: CapabilityMetrics::default(),
            cluster: Some(ClusterMetrics {
                nodes: 3,
                healthy: true,
                quorum_writable: true,
                under_replicated_objects: 0,
                repair_active_tasks: 0,
                node_capacity_bytes: 0,
                node_used_bytes: 0,
                node_available_bytes: 0,
                logical_bytes: 0,
                physical_bytes: 0,
                devices: 8,
                devices_accepting_placement: 6,
                devices_draining: 1,
                devices_failed: 1,
                devices_unavailable: 0,
                device_capacity_raw_bytes: 8_000,
                device_capacity_usable_bytes: 7_200,
                device_capacity_available_bytes: 3_000,
            }),
        };

        let body = prometheus_exposition(&snapshot);
        for (metric, value) in [
            ("record_store_devices_total", "8"),
            ("record_store_devices_accepting_placement", "6"),
            ("record_store_devices_draining", "1"),
            ("record_store_devices_failed", "1"),
            ("record_store_devices_unavailable", "0"),
            ("record_store_device_capacity_raw_bytes", "8000"),
            ("record_store_device_capacity_usable_bytes", "7200"),
            ("record_store_device_capacity_available_bytes", "3000"),
        ] {
            assert!(
                body.contains(&format!("{metric} {value}\n")),
                "expected `{metric} {value}` in the scrape"
            );
        }

        for line in body
            .lines()
            .filter(|line| line.starts_with("record_store_device"))
        {
            assert!(
                !line.contains('{'),
                "device metrics must carry no labels, found: {line}"
            );
        }
    }

    /// The console reads the same numbers Prometheus scrapes. Gathering once and
    /// rendering twice is what keeps the two views from drifting apart, so both
    /// have to report the same counts.
    #[tokio::test]
    async fn the_json_and_prometheus_views_report_the_same_numbers() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        put_object(&api, "photos", "a.txt", b"hello").await;

        let json = expect_status(
            &api,
            admin("GET", "/api/v1/system/metrics", None),
            StatusCode::OK,
        )
        .await;
        let buckets = json["storage"]["bucket_count"]
            .as_u64()
            .expect("bucket count in the JSON view");
        assert_eq!(buckets, 1, "{json}");

        let scrape = call(&api, signed("GET", "/metrics", METRICS_TOKEN, None)).await;
        assert_eq!(scrape.status(), StatusCode::OK);
        let exposition = text(scrape).await;
        assert!(
            exposition.contains(&format!("record_store_buckets {buckets}"))
                || exposition.contains(&buckets.to_string()),
            "the scrape must carry the same count: {exposition}"
        );
    }

    /// Prometheus needs a TYPE line for every series it is offered, or the
    /// sample is ignored by the scraper without any error.
    #[tokio::test]
    async fn every_exposed_series_declares_its_type() {
        let (_directory, api) = api().await;
        let scrape = call(&api, signed("GET", "/metrics", METRICS_TOKEN, None)).await;
        assert_eq!(scrape.status(), StatusCode::OK);
        let exposition = text(scrape).await;

        let mut declared = std::collections::BTreeSet::new();
        let mut sampled = std::collections::BTreeSet::new();
        for line in exposition.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                declared.insert(rest.split_whitespace().next().expect("name").to_owned());
            } else if !line.starts_with('#') && !line.trim().is_empty() {
                sampled.insert(line.split_whitespace().next().expect("name").to_owned());
            }
        }
        assert!(!sampled.is_empty(), "{exposition}");
        let undeclared: Vec<_> = sampled.difference(&declared).collect();
        assert!(
            undeclared.is_empty(),
            "these series carry no TYPE line: {undeclared:?}"
        );
    }

    /// Scraping uses a credential of its own, separate from the management
    /// tokens. A management token must not open the scrape endpoint, and an
    /// anonymous scrape must not either — Prometheus data still describes the
    /// deployment's contents.
    #[tokio::test]
    async fn scraping_requires_its_own_dedicated_credential() {
        let (_directory, api) = api().await;

        let anonymous = call(
            &api,
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let with_management_token = call(&api, admin("GET", "/metrics", None)).await;
        assert_eq!(
            with_management_token.status(),
            StatusCode::UNAUTHORIZED,
            "a management token is not a scrape token"
        );

        let scrape = call(&api, signed("GET", "/metrics", METRICS_TOKEN, None)).await;
        assert_eq!(scrape.status(), StatusCode::OK);
    }

    /// The JSON view is management data, so it must not be readable without a
    /// credential even though the scrape endpoint is.
    #[tokio::test]
    async fn the_json_view_still_requires_a_credential() {
        let (_directory, api) = api().await;
        let response = call(
            &api,
            Request::builder()
                .method("GET")
                .uri("/api/v1/system/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Counts have to follow real activity rather than being reported as zero
    /// forever, which is the failure mode nobody notices until an incident.
    #[tokio::test]
    async fn stored_objects_are_reflected_in_the_metrics() {
        let (_directory, api) = api().await;
        make_bucket(&api, "photos").await;
        for key in ["a.txt", "b.txt"] {
            put_object(&api, "photos", key, b"hello").await;
        }

        let json = expect_status(
            &api,
            admin("GET", "/api/v1/system/metrics", None),
            StatusCode::OK,
        )
        .await;
        assert_eq!(json["storage"]["object_count"], 2, "{json}");
        assert!(
            json["storage"]["logical_bytes"].as_u64().expect("bytes") >= 10,
            "{json}"
        );
    }
}
