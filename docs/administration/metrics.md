# Metrics

Record Store exposes the same numbers two ways: Prometheus text on `/metrics`, and
JSON on `/api/v1/system/metrics` for the web console.

The values are gathered once and rendered twice, so the two views cannot drift apart.

## Enabling the scrape endpoint

`/metrics` requires a dedicated token and is **closed when none is configured**. There
is no unauthenticated mode.

```bash
RECORD_STORE_METRICS_SCRAPE_TOKEN=<your-metrics-token>
```

The token must be 32–1024 visible ASCII characters and must differ from every
management role token. That separation is the point: a scrape credential lives in a
monitoring system's configuration and should carry no authority over management
routes, and the console — which holds a management token — deliberately cannot read
`/metrics`.

## Scraping

```bash
curl https://management.example.com/metrics \
  -H "Authorization: Bearer <your-metrics-token>"
```

```yaml
scrape_configs:
  - job_name: record-store
    metrics_path: /metrics
    scheme: https
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/record-store-token
    static_configs:
      - targets: ["management.example.com"]
```

`/metrics` is served on the management port (7601), which should not be publicly
reachable. See [Ports](../reference/ports.md).

## Metrics

### Requests

| Metric | Type | Meaning |
| --- | --- | --- |
| `record_store_requests_total` | counter | Requests served since process start |
| `record_store_s3_requests_total` | counter | Same value, kept for existing scrapers |
| `record_store_errors_total` | counter | Requests that failed |
| `record_store_upload_bytes_total` | counter | Bytes accepted from clients |
| `record_store_download_bytes_total` | counter | Bytes served to clients |

These are process-lifetime counters. They reset on restart — use `rate()` and
`increase()` rather than reading the raw value.

### Storage

| Metric | Type | Meaning |
| --- | --- | --- |
| `record_store_buckets_total` | gauge | Buckets |
| `record_store_objects_total` | gauge | Current objects |
| `record_store_versions_total` | gauge | Object versions, including current |
| `record_store_storage_logical_bytes` | gauge | Logical bytes |
| `record_store_storage_bytes` | gauge | Same value, kept for existing scrapers |
| `record_store_storage_physical_bytes` | gauge | Bytes actually occupied |
| `record_store_multipart_bytes` | gauge | Bytes held by in-progress multipart uploads |

The gap between logical and physical bytes is version history and multipart parts.
Watch physical bytes for capacity, logical bytes for what users think they have. See
[Capacity Planning](../operations/capacity-planning.md).

### Sharing and previews

| Metric | Type | Meaning |
| --- | --- | --- |
| `record_store_preview_requests_total` | counter | Console preview requests |
| `record_store_preview_failures_total` | counter | Preview requests that failed |
| `record_store_share_links_created_total` | counter | Share links created |
| `record_store_share_access_total` | counter | Successful share accesses |
| `record_store_share_access_denied_total` | counter | Refused share accesses |
| `record_store_share_links_active` | gauge | Share links currently valid |
| `record_store_embeds_created_total` | counter | Embed links created |
| `record_store_embed_requests_total` | counter | Successful embed requests |
| `record_store_embed_denied_total` | counter | Refused embed requests |
| `record_store_embeds_active` | gauge | Embed links currently valid |

Successful public accesses are counted here rather than written to the
[audit log](audit-log.md) — a shared video would otherwise let an anonymous visitor
fill the security trail. The denial counters are the ones worth alerting on.

### Cluster

Present **only in cluster mode**. A standalone deployment omits them rather than
reporting zeroes that look like a broken cluster.

| Metric | Type | Meaning |
| --- | --- | --- |
| `record_store_cluster_nodes` | gauge | Nodes known to the cluster |
| `record_store_node_health` | gauge | `1` when this node is healthy |
| `record_store_metadata_quorum_health` | gauge | `1` when metadata quorum is writable |
| `record_store_under_replicated_objects` | gauge | Objects below their replication factor |
| `record_store_replication_queue_depth` | gauge | Repair tasks currently running |
| `record_store_node_capacity_bytes` | gauge | This node's total capacity |
| `record_store_node_used_bytes` | gauge | This node's used bytes |
| `record_store_node_available_bytes` | gauge | This node's available bytes |
| `record_store_cluster_logical_bytes` | gauge | Cluster-wide logical bytes |
| `record_store_cluster_physical_bytes` | gauge | Cluster-wide physical bytes |

### Devices

Also cluster-mode only.

| Metric | Type | Meaning |
| --- | --- | --- |
| `record_store_devices_total` | gauge | Registered devices cluster-wide |
| `record_store_devices_accepting_placement` | gauge | Devices eligible for new data |
| `record_store_devices_draining` | gauge | Devices being evacuated |
| `record_store_devices_failed` | gauge | Devices whose data no longer counts for durability |
| `record_store_devices_unavailable` | gauge | Devices held out of service by an administrator |
| `record_store_device_capacity_raw_bytes` | gauge | Raw capacity across all devices |
| `record_store_device_capacity_usable_bytes` | gauge | Capacity Record Store may allocate from |
| `record_store_device_capacity_available_bytes` | gauge | Capacity currently free |

!!! note "Counts, not one series per device"
    Device metrics carry **no labels**. A series per device would grow without
    bound as hardware is replaced, so the scrape reports counts by state and
    summed capacity. To see individual devices, use
    `record-store drive list` or `GET /api/v1/devices`.

`record_store_devices_failed` counts a device that either an administrator marked
failed **or** whose health the platform reports as failed. The two are recorded
separately and either one is disqualifying.

!!! note "`record_store_replication_queue_depth`"
    The name predates what it now reports: active repair tasks. It is kept as-is so
    existing dashboards keep working.

## Alerts worth having

```yaml
groups:
  - name: record-store
    rules:
      - alert: RecordStoreMetadataQuorumLost
        expr: record_store_metadata_quorum_health == 0
        for: 1m
        annotations:
          summary: Metadata quorum is not writable

      - alert: RecordStoreUnderReplicated
        expr: record_store_under_replicated_objects > 0
        for: 15m
        annotations:
          summary: Objects have been below their replication factor for 15 minutes

      - alert: RecordStoreDiskNearlyFull
        expr: record_store_node_available_bytes / record_store_node_capacity_bytes < 0.1
        for: 5m
        annotations:
          summary: Less than 10% of node capacity remains

      - alert: RecordStoreErrorRate
        expr: rate(record_store_errors_total[5m]) / rate(record_store_requests_total[5m]) > 0.05
        for: 10m
        annotations:
          summary: Over 5% of requests are failing
```

The `for:` clauses matter. Under-replication during a rolling restart is expected and
resolves itself; alerting instantly produces noise that gets muted, which is worse
than no alert.

## JSON view

The console reads the same values through the management API:

```bash
curl https://management.example.com/api/v1/system/metrics \
  -H "Authorization: Bearer <your-management-token>"
```

Use this when you want the numbers in a script and already hold a management token.
The `cluster` object is omitted entirely in standalone mode.

## What is not here

There are no per-bucket, per-operation, or per-status-code metrics, and no latency
histograms. For request-level detail use the [audit log](audit-log.md) — filtered by
`operation` and `result` — or the structured logs described in
[Monitoring](../operations/monitoring.md).
