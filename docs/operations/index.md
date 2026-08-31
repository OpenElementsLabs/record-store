# Operations

Keeping a deployment healthy and knowing when it is not.

<div class="grid cards" markdown>

-   **[Health and Readiness](health-and-readiness.md)** — the probes and what they mean
-   **[Monitoring](monitoring.md)** — metrics, logs, and what to alert on
-   **[Backup and Restore](backup-and-restore.md)** — taking backups that work
-   **[Integrity Verification](integrity-verification.md)** — proving the bytes are intact
-   **[Capacity Planning](capacity-planning.md)** — logical, physical, and what grows
-   **[Disaster Recovery](disaster-recovery.md)** — when things go badly wrong

</div>

## Daily

```bash
record-store status --endpoint https://management.example.com
record-store storage inspect --endpoint https://management.example.com
```

If those two look right, the deployment is fine. If they do not, the rest of this
section says what to do.

## Signals worth watching

| Signal | Where | Concerning when |
| --- | --- | --- |
| Readiness | `/ready` | Anything but 200 |
| Disk headroom | A host exporter on the data directory's filesystem | Below 20% |
| Error rate | `record_store_errors_total` / `record_store_requests_total` | Above a few percent |
| Missing payloads | `metadata_without_data` in `storage inspect` | Above 0 |
| Storage growth | `record_store_storage_physical_bytes` | Outpacing the disk you have |
| Audit denials | `record-store audit --limit 100` | A burst from one principal |

## Nothing is pruned for you

Two stores grow without bound and have no automatic retention:

- **The audit trail** grows with request volume.
- **Version history** grows with every overwrite on a versioned bucket.

Lifecycle rules handle the second. The first is a capacity-planning input. See
[Capacity Planning](capacity-planning.md).
