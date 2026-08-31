# Management API

The native administrative API, served on port **7601**.

## Base

```text
https://management.example.com/api/v1
```

## Authentication

```bash
curl https://management.example.com/api/v1/buckets \
  -H "Authorization: Bearer <your-management-token>"
```

HTTP Basic with the root credential is also accepted and grants the
system-administrator role. See [Authentication](../security/authentication.md).

Every route below requires a management token unless marked otherwise. Which routes a
token may call depends on its role — see [Authorization](../security/authorization.md).

## Errors

```json
{
  "error": {
    "code": "BUCKET_NOT_FOUND",
    "message": "Bucket was not found",
    "request_id": "..."
  }
}
```

The request ID is also in the `x-request-id` response header, and an inbound
`x-request-id` is reused. See [Error Reference](errors.md).

## Unauthenticated

| Method | Path | Returns |
| --- | --- | --- |
| `GET` | `/health` | `{"status":"ok"}` |
| `GET` | `/ready` | `{"status":"ready"}`, or `503` |

`GET /metrics` requires the dedicated scrape token, not a management token.

## System

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/system/info` | Version and capabilities |
| `GET` | `/api/v1/system/metrics` | The same values `/metrics` exposes, as JSON |
| `GET` | `/api/v1/auth/session` | The role behind the presented credential |

## Buckets

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/buckets` | List buckets |
| `POST` | `/api/v1/buckets` | Create — `{"name":"..."}` |
| `DELETE` | `/api/v1/buckets/{bucket}` | Delete an empty bucket |
| `GET` | `/api/v1/buckets/{bucket}/versioning` | Versioning state |
| `PUT` | `/api/v1/buckets/{bucket}/versioning` | Set versioning |
| `PUT` | `/api/v1/buckets/{bucket}/quota` | Set a quota |

## Objects

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/buckets/{bucket}/objects` | List objects |
| `GET` | `/api/v1/buckets/{bucket}/object/{key}` | Object metadata |
| `PUT` | `/api/v1/buckets/{bucket}/object/{key}` | Upload — body is the object |
| `DELETE` | `/api/v1/buckets/{bucket}/object/{key}` | Delete |
| `GET` | `/api/v1/buckets/{bucket}/object-content/{key}` | Download bytes |
| `GET` | `/api/v1/buckets/{bucket}/object-preview/{key}` | Console preview |
| `GET` | `/api/v1/buckets/{bucket}/object-versions` | List versions |
| `DELETE` | `/api/v1/buckets/{bucket}/object-versions/{key}` | Delete a version |
| `POST` | `/api/v1/buckets/{bucket}/object-copy/{key}` | Server-side copy |
| `POST` | `/api/v1/restore/{bucket}/{key}` | Restore a version as current |

The upload route streams, so the small-payload body limit that protects the JSON routes
does not apply to it.

`POST /api/v1/restore/...` takes `{"version_id":"..."}` and returns `201`. It creates a
**new current version** rather than moving a pointer — history is preserved.

Applications should use the [S3 API](s3-compatibility.md). These routes exist for the
console and for administration.

## Service accounts and credentials

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/service-accounts` | List |
| `POST` | `/api/v1/service-accounts` | Create — `{"name":"...","description":"..."}` |
| `GET` | `/api/v1/service-accounts/{id}` | Inspect |
| `DELETE` | `/api/v1/service-accounts/{id}` | Delete permanently |
| `PUT` | `/api/v1/service-accounts/{id}/status` | `{"enabled":true|false}` |
| `POST` | `/api/v1/service-accounts/{id}/credentials` | Rotate — issues a new credential |
| `POST` | `/api/v1/service-accounts/{id}/temporary-credentials` | `{"expires_in_seconds":3600}` |
| `PUT` | `/api/v1/service-accounts/{id}/credentials/{credential_id}/status` | `{"enabled":...}` |

Creation and rotation return the secret key **once** and require
`auth.credential_master_key`.

Rotation issues a new credential alongside the old one; the old one keeps working until
disabled.

## Policies

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/policies` | List |
| `POST` | `/api/v1/policies` | Create |
| `PUT` | `/api/v1/policies/{policy_id}/bindings/{account_id}` | Attach |
| `DELETE` | `/api/v1/policies/{policy_id}/bindings/{account_id}` | Detach |

## Lifecycle

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/buckets/{bucket}/lifecycle` | List rules |
| `POST` | `/api/v1/buckets/{bucket}/lifecycle` | Create a rule |
| `PUT` | `/api/v1/buckets/{bucket}/lifecycle/{rule_id}` | Replace a rule |
| `DELETE` | `/api/v1/lifecycle-rules/{id}` | Delete a rule |

Updates are complete replacements. See
[Lifecycle Rules](../administration/lifecycle-rules.md).

## Events and webhooks

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/events` | Storage events |
| `GET` | `/api/v1/webhooks` | List webhooks |
| `POST` | `/api/v1/webhooks` | Create — returns the signing secret once |
| `PUT` | `/api/v1/webhooks/{id}/status` | `{"enabled":...}` |
| `DELETE` | `/api/v1/webhooks/{id}` | Delete |
| `GET` | `/api/v1/webhook-deliveries` | Delivery log — `limit` defaults to 100 |

## Audit

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/audit/events` | Query the audit trail |

Parameters: `since`, `until`, `principal`, `operation`, `resource`, `result`,
`source_ip`, `request_id`, `after_time`, `after_id`, `limit` (1–1000, default 100).

`after_time` and `after_id` must be supplied together.

## Storage

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/storage/status` | Capacity and available bytes |
| `GET` | `/api/v1/storage/usage` | Object, bucket, and version accounting |
| `GET` | `/api/v1/storage/inspect` | Structural consistency — `maximum_entries` |
| `POST` | `/api/v1/storage/repair` | `{"maximum_entries":N,"dry_run":true}` |

`dry_run` defaults to **`true`**. Send `false` to actually remove orphans.

## Integrity

| Method | Path | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/verify/objects/{bucket}/{key}` | Verify one object |
| `POST` | `/api/v1/verify/buckets/{bucket}` | Verify every object in a bucket |

## Sharing

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/v1/sharing/settings` | Deployment-wide sharing policy |
| `GET` | `/api/v1/buckets/{bucket}/object-shares/{key}` | Shares on an object |
| `POST` | `/api/v1/buckets/{bucket}/object-shares/{key}` | Create a share |
| `GET` | `/api/v1/shares/{id}` | Share metadata |
| `GET` | `/api/v1/shares/{id}/url` | **The share URL** |
| `POST` | `/api/v1/shares/{id}/revoke` | Revoke |
| `DELETE` | `/api/v1/shares/{id}` | Delete |
| `GET` | `/api/v1/buckets/{bucket}/object-embeds/{key}` | Embeds on an object |
| `POST` | `/api/v1/buckets/{bucket}/object-embeds/{key}` | Create an embed |
| `GET` | `/api/v1/embeds/{id}` | Embed metadata |
| `PATCH` | `/api/v1/embeds/{id}` | Update an embed |
| `GET` | `/api/v1/embeds/{id}/url` | **The embed URL** |
| `POST` | `/api/v1/embeds/{id}/revoke` | Revoke |
| `DELETE` | `/api/v1/embeds/{id}` | Delete |

The `/url` routes return the capability itself and are refused to the auditor role.

## Public capability delivery

No management token. The token in the path is the entire authorization, re-checked
against durable state on every request.

| Method | Path | Served on |
| --- | --- | --- |
| `GET` | `/s/{token}` | 7601 and 7602 — share descriptor |
| `POST` | `/s/{token}/unlock` | Share password unlock |
| `GET` | `/s/{token}/content` | Share content |
| `GET` | `/e/{token}` | **7600** — embed content |

Embed delivery is on the storage data plane because an embed serves object bytes.

## Body limits

JSON routes accept up to 1 MiB. The object upload route streams and is exempt.
