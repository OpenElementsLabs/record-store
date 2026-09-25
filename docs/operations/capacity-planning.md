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
`record_store_storage_physical_bytes`, `record_store_multipart_bytes`,
`record_store_temporary_bytes`, `record_store_filesystem_capacity_bytes`, and
`record_store_filesystem_available_bytes`.

The filesystem figures are read from the filesystem itself, so they account for
everything on it, not only what Record Store put there.

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

## Memory

The server's memory has a part that follows load and a part that follows history.

- **Load**: requests in flight, their buffers, and the threads serving them. It rises
  and falls with traffic and is bounded by `limits.maximum_concurrent_operations`.
- **History**: the page cache of the metadata databases. The catalog, the audit trail
  and the storage-event journal grow with every request, and redb caches their pages
  as it reads and writes them. `storage.metadata_cache_mib` (default 128) is shared
  between those three — half for the catalog, a quarter each for the others — and the
  credential, sharing and lifecycle databases keep 16 MiB each. Once the databases
  outgrow it, this part stops growing.

Up to 0.1.3, every database cached up to 1 GiB, so memory followed the database files
until each cache was full — about 1.3 KB per request under a steady mixed workload.
Measured on Linux with that workload (16 clients, 200 keys, 70 % reads, 1–256 KiB
objects), anonymous memory of the server container with a 384 MiB limit:

| Minutes | 0.1.3-style caches | Bounded cache, glibc defaults | Bounded cache, `MALLOC_ARENA_MAX=2` |
| --- | --- | --- | --- |
| 5 | 174 MiB | 108 MiB | 67 MiB |
| 10 | 291 MiB | 148 MiB | 75 MiB |
| 14 | 374 MiB, then killed by the limit | 174 MiB | 87 MiB |
| 30 | — | 205 MiB | 99 MiB |
| 60 | — | 232 MiB | — |

With the cache bounded, memory levels off; what continues to rise slowly afterwards
is glibc's allocator keeping memory in per-thread arenas, not live data. A heap
profile of the bounded server over 12 minutes found a peak of 66 MB of live
allocations, most of it the audit and event caches, and 0.4 MB unreleased at exit.
Capping glibc at two arenas halves resident memory with no measurable change in
throughput, so **the container image sets `MALLOC_ARENA_MAX=2`**, and so does
anything built on it, the Helm chart and the Compose files included. Set it yourself
when you run the glibc binary archive directly. The static (`-musl`) binaries in the
archives and the Debian and RPM packages use musl's allocator, which the variable does
not affect.

To size a container or a unit's `MemoryMax`, start from what was measured: with the
default cache and the image's settings the server levelled off near 100 MiB, with
glibc's defaults near 230 MiB, and both ran under a 384 MiB limit that 0.1.3's caches
exceeded within a quarter of an hour. Allow at least 384 MiB, plus whatever you add to
`metadata_cache_mib`, and more if your clients hold many large transfers open at
once. The Helm chart requests 512 MiB and limits the pod to 2 GiB. A read that misses the cache is served from the
operating system's file cache or the disk, so a small cache costs latency on a large
catalog, not correctness.

The `RES-MEMORY` release gate holds this: with an 8 MiB cache, after the caches have
filled, resident memory may grow by at most 256 bytes per request — a fifth of what
the unbounded cache produced.

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
