# Administration

Managing a running Record Store deployment: credentials, access control, retention,
and the trails that tell you what happened.

<div class="grid cards" markdown>

-   **[Configuration](configuration.md)** — how settings are loaded and layered
-   **[Service Accounts](service-accounts.md)** — application credentials
-   **[Policies](policies.md)** — what an account may do
-   **[Temporary Credentials](temporary-credentials.md)** — expiring credentials
-   **[Quotas](quotas.md)** — bounding a bucket's size
-   **[Lifecycle Rules](lifecycle-rules.md)** — expiring objects on a schedule
-   **[Audit Log](audit-log.md)** — administrative history
-   **[Events and Webhooks](events-and-webhooks.md)** — storage events, delivered
-   **[Metrics](metrics.md)** — Prometheus scraping

</div>

## Three trails, three purposes

They are separate on purpose. Merging them loses the distinction that makes each
useful.

| | Contains | Read it when |
| --- | --- | --- |
| [Audit](audit-log.md) | Security and administrative actions | Asking *who changed what* |
| [Events](events-and-webhooks.md) | Storage changes to buckets and objects | Reacting to data changes |
| [Logs](../operations/monitoring.md#logs) | Operational diagnostics | Debugging the process |

## Management roles

Management authentication is separate from S3 access. A management token is not an S3
credential and cannot read objects.

| Role | Token | Can do |
| --- | --- | --- |
| System administrator | `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` | Everything |
| Storage administrator | `RECORD_STORE_MANAGEMENT_STORAGE_TOKEN` | Buckets, objects, storage, integrity, lifecycle |
| Auditor | `RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN` | Read-only, including the audit trail |

See [Authorization](../security/authorization.md).
