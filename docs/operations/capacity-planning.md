# Capacity Planning

## What consumes space

| | Grows with | Pruned by |
| --- | --- | --- |
| Object payloads | Your data | Deletion |
| Version history | Overwrites on versioned buckets | [Lifecycle rules](../administration/lifecycle-rules.md) |
| Multipart parts | Uploads not completed or aborted | Completion or abort |
| Metadata | Object **count**, not size | Nothing |
| Audit trail | Request volume | **Nothing** |
| Consensus log | Metadata write volume | Snapshots and compaction |

Two of those have no automatic retention: metadata and the audit trail. Budget for
them.

## Logical versus physical

```bash
record-store storage inspect --endpoint https://management.example.com
```

| | Means |
| --- | --- |
| **Logical bytes** | What users think they have — current object versions |
| **Physical bytes** | What the disk actually holds |

The gap is version history plus multipart parts. On a versioned bucket that is
overwritten often, physical can be several times logical.

Quotas enforce on **logical** bytes. A bucket can therefore stay well inside its quota
while its physical footprint keeps growing. Watch both.

## Disk usage

```bash
curl https://management.example.com/api/v1/storage/status \
  -H "Authorization: Bearer <your-management-token>"
```

```json
{
  "capacity_bytes": 1099511627776,
  "available_bytes": 549755813888,
  "temporary_upload_bytes": 1073741824
}
```

`temporary_upload_bytes` is space held by in-flight uploads. A persistently large value
means multipart uploads are being started and not finished — see
[Multipart Uploads](../guides/multipart-uploads.md).

Prometheus equivalents: `record_store_storage_logical_bytes`,
`record_store_storage_physical_bytes`, `record_store_multipart_bytes`, and in a cluster
`record_store_node_available_bytes`.

## Sizing a standalone deployment

```text
disk = payloads
     + version history
     + in-flight multipart
     + metadata
     + audit trail
     + headroom
```

Rules of thumb:

- **Version history**: on a versioned bucket, budget for the number of versions your
  lifecycle rules retain, times average object size.
- **Metadata**: grows with object count. A million small objects costs far more metadata
  than a thousand large ones of the same total size.
- **Audit trail**: grows with request volume and is never pruned. A high-traffic
  deployment accumulates it steadily.
- **Headroom**: keep at least 20 percent free. Writes fail at zero, and repair and
  rebalance need somewhere to put things.

Measure your own ratios rather than trusting an estimate — run for a week and read
`storage inspect`.

## Sizing a cluster

Total raw capacity needed is roughly:

```text
raw = logical × replication_factor / target_utilization
```

With a replication factor of 3 and a target of 70 percent utilization, 1 TB of logical
data needs about 4.3 TB raw.

Also:

- **Every node needs headroom.** A node at the critical watermark stops accepting new
  placement, which pushes load onto the others.
- **Losing a node means its replicas are rebuilt elsewhere.** The remaining nodes need
  space for that. Size for `n-1`.
- **Failure domains need capacity too.** Three replicas across three racks means each
  rack needs a third of the total.

Watermarks:

```bash
RECORD_STORE_CLUSTER_CAPACITY_LOW_WATERMARK_PERCENT=80
RECORD_STORE_CLUSTER_CAPACITY_HIGH_WATERMARK_PERCENT=90
RECORD_STORE_CLUSTER_CAPACITY_CRITICAL_WATERMARK_PERCENT=95
```

## Bounding growth

**Lifecycle rules** for version history and old objects:

```bash
curl -X POST https://management.example.com/api/v1/buckets/logs/lifecycle \
  -H "Authorization: Bearer <your-management-token>" \
  -H "Content-Type: application/json" \
  -d '{"prefix":"","expiration":90,"noncurrent_version_expiration":7}'
```

On a versioned bucket, `expiration` alone reclaims nothing — it writes a delete marker.
Pair it with `noncurrent_version_expiration` to actually recover space.

**Quotas** to stop one bucket consuming the deployment:

```bash
curl -X PUT https://management.example.com/api/v1/buckets/uploads/quota \
  -H "Authorization: Bearer <your-management-token>" \
  -H "Content-Type: application/json" \
  -d '{"quota":{"bytes":{"mode":"limit","bytes":107374182400},"objects":{"mode":"unlimited"}}}'
```

**Orphan cleanup** to recover space nothing references:

```bash
record-store storage repair --endpoint https://management.example.com          # dry run
record-store storage repair --apply --endpoint https://management.example.com
```

## When a disk fills

Writes fail. Reads continue.

Immediate options, cheapest first:

1. `storage repair --apply` — removes orphaned payloads.
2. Abort stale multipart uploads if `temporary_upload_bytes` is large.
3. Run lifecycle rules more aggressively — lower `interval_seconds`, raise `batch_size`.
4. Delete data you can identify as disposable.
5. Add capacity — a bigger disk standalone, another node in a cluster.

In a cluster, a full node stops accepting placement but keeps serving reads. Adding a
node and rebalancing is the durable fix.

## Alerting

```yaml
- alert: RecordStoreDiskNearlyFull
  expr: record_store_node_available_bytes / record_store_node_capacity_bytes < 0.2
  for: 10m

- alert: RecordStoreDiskCritical
  expr: record_store_node_available_bytes / record_store_node_capacity_bytes < 0.1
  for: 1m
```

Alert at 20 percent, not at 5. Adding storage takes time, and lifecycle rules take
hours to drain a backlog.
