# Monitoring

Three sources, three purposes.

| | Contains | Ask it |
| --- | --- | --- |
| [Metrics](#metrics) | Counters and gauges | Is anything wrong right now? |
| [Logs](#logs) | Per-request diagnostics | Why did this request behave that way? |
| [Audit](../administration/audit-log.md) | Security and administrative history | Who did what, and when? |

## Metrics

Prometheus text on `/metrics`, behind a dedicated scrape token:

```bash
RECORD_STORE_METRICS_SCRAPE_TOKEN=<your-metrics-token>
```

```bash
curl https://management.example.com/metrics \
  -H "Authorization: Bearer <your-metrics-token>"
```

The complete metric list, and suggested alert rules, are in
[Metrics](../administration/metrics.md).

The four that matter most:

| Metric | Watch for |
| --- | --- |
| `record_store_node_available_bytes` | Falling toward zero |
| `record_store_errors_total` | A rising rate relative to requests |
| `record_store_metadata_quorum_health` | Dropping to `0` |
| `record_store_under_replicated_objects` | Staying above `0` |

In a cluster, watch device health too:

| Metric | Watch for |
| --- | --- |
| `record_store_devices_failed` | Any value above `0` |
| `record_store_devices_accepting_placement` | Falling toward `0` while `record_store_devices_total` holds steady |
| `record_store_device_capacity_available_bytes` | Falling faster than capacity is added |

## Logs

Structured through `tracing`.

```toml
[observability]
log_filter = "record_store=info"
json = false
```

```bash
RECORD_STORE_LOG=record_store=info
RECORD_STORE_LOG_JSON=true
```

`json = true` emits newline-delimited JSON with the current span and span list
included, which is what you want when a collector parses it. The container image
defaults to JSON for exactly that reason.

### Filter syntax

`log_filter` is a `tracing-subscriber` filter directive:

| Value | Effect |
| --- | --- |
| `record_store=info` | Default |
| `record_store=debug` | Verbose, for investigation |
| `record_store=warn` | Quiet |
| `record_store=info,record_store_s3=debug` | Info overall, debug for the S3 adapter |
| `record_store=info,record_store_cluster=debug` | Info overall, debug for cluster internals |

An invalid filter is rejected before the subscriber is installed, so a typo fails at
startup rather than silently disabling logging.

The filter is read once at startup. Changing it means a restart.

### Request logs

Every request produces an `http.request` span and a `request completed` event carrying
the request ID, method, route, status, and latency in milliseconds.

Routes are logged as **route patterns**, not raw paths, so bucket names and object keys
do not end up in the log.

### Request IDs

| Plane | Response header |
| --- | --- |
| Management API | `x-request-id` |
| S3 API | `x-amz-request-id` |

The management API accepts an inbound `x-request-id` and reuses it, so a trace ID from
your gateway carries through.

The same ID appears on the log line and on the audit event, which makes it the fastest
path from a user's error report to what the server actually decided:

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "request_id=<request id from the response header>"
```

### Secrets in logs

Secret-typed configuration renders as `<redacted>`, and a configuration parse failure
names the variable without printing its value. Webhook delivery logs record a bounded
error summary and never the response body.

### Collecting

```bash
docker compose logs -f record-store
docker compose logs record-store | grep '"level":"ERROR"'
```

With JSON logging, any collector that parses NDJSON works — Loki, Elasticsearch,
CloudWatch, Vector. Index on `request_id`, `status`, and `route`.

## Storage inspection

```bash
record-store storage inspect --endpoint https://management.example.com
```

Reports counts and byte totals, including the split between logical and physical bytes.
`--maximum-entries` bounds the scan (default 100000).

See [Capacity Planning](capacity-planning.md).

## Cluster

```bash
record-store cluster status --endpoint https://management.example.com
record-store repair status --endpoint https://management.example.com
record-store rebalance status --endpoint https://management.example.com
```

## A monitoring setup that works

1. Prometheus scraping `/metrics` with its token.
2. Alerts on quorum, under-replication, disk headroom, and error rate — each with a
   `for:` clause so transient states do not page.
3. Logs collected as JSON, indexed on `request_id` and `status`.
4. A dashboard showing request rate, error rate, storage growth, and — in a cluster —
   node health.
5. A weekly look at audit denials.

The `for:` clauses matter more than the thresholds. Under-replication during a rolling
restart is expected; an alert that fires instantly gets muted, and a muted alert is
worse than none.
