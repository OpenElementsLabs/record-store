# Capacity Planning

## What consumes space

| | Grows with | Pruned by |
| --- | --- | --- |
| Object payloads | Your data | Deletion |
| Version history | Overwrites on versioned buckets | [Lifecycle rules](../administration/lifecycle-rules.md) |
| Multipart parts | Uploads not completed or aborted | Completion or abort |
| Metadata | Object **count**, not size | Nothing |
| Audit trail | Request volume | **Nothing** |

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
`record_store_storage_physical_bytes`, and `record_store_multipart_bytes`. None of
them reports free disk — that comes from a host exporter.

## Sizing the disk

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
- **Headroom**: keep at least 20 percent free. Writes fail at zero, and every recovery
  option needs somewhere to put things.

Measure your own ratios rather than trusting an estimate — run for a week and read
`storage inspect`.

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
5. Add capacity — a bigger disk, or grow the volume the data directory sits on.

Growing the underlying storage is the durable fix; the rest buy time.

## Alerting

Free space is a host metric, so alert on it from a node exporter watching the
filesystem the data directory sits on:

```yaml
- alert: RecordStoreDiskNearlyFull
  expr: node_filesystem_avail_bytes{mountpoint="/var/lib/record-store"}
        / node_filesystem_size_bytes{mountpoint="/var/lib/record-store"} < 0.2
  for: 10m

- alert: RecordStoreDiskCritical
  expr: node_filesystem_avail_bytes{mountpoint="/var/lib/record-store"}
        / node_filesystem_size_bytes{mountpoint="/var/lib/record-store"} < 0.1
  for: 1m
```

Alert at 20 percent, not at 5. Adding storage takes time, and lifecycle rules take
hours to drain a backlog.
