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

## Alerts worth having

```yaml
groups:
  - name: record-store
    rules:
      - alert: RecordStoreDown
        expr: up{job="record-store"} == 0
        for: 2m
        annotations:
          summary: Record Store is not being scraped

      - alert: RecordStoreErrorRate
        expr: rate(record_store_errors_total[5m]) / rate(record_store_requests_total[5m]) > 0.05
        for: 10m
        annotations:
          summary: Over 5% of requests are failing

      - alert: RecordStoreShareDenials
        expr: rate(record_store_share_access_denied_total[5m]) > 1
        for: 10m
        annotations:
          summary: Sustained refused share-link access
```

The `for:` clauses matter. A brief spike during a restart resolves itself; alerting
instantly produces noise that gets muted, which is worse than no alert.

!!! note "Free disk space is not a Record Store metric"
    `record_store_storage_bytes` is what Record Store has stored, not what the
    filesystem has left. Alert on free space with a host exporter — Record Store
    does not report the disk's own capacity. See
    [Capacity Planning](../operations/capacity-planning.md).

## JSON view

The console reads the same values through the management API:

```bash
curl https://management.example.com/api/v1/system/metrics \
  -H "Authorization: Bearer <your-management-token>"
```

Use this when you want the numbers in a script and already hold a management token.

## What is not here

There are no per-bucket, per-operation, or per-status-code metrics, and no latency
histograms. For request-level detail use the [audit log](audit-log.md) — filtered by
`operation` and `result` — or the structured logs described in
[Monitoring](../operations/monitoring.md).
