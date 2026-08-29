# Authorization

Two separate systems, because the planes are separate.

| Plane | Mechanism |
| --- | --- |
| S3 API | Policies attached to service accounts |
| Management API | Three fixed roles, checked per route |

## S3 authorization

Policies grant actions on `bucket:` resources. Default deny; explicit deny wins;
policies are additive.

Root and system principals bypass evaluation entirely.

Full detail — actions, resource patterns, evaluation order, and worked examples — is in
[Policies](../administration/policies.md).

## Management roles

Three roles, fixed. They are not configurable and cannot be composed.

### System administrator

`RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN`

Everything. Required if either other role token is configured.

### Storage administrator

`RECORD_STORE_MANAGEMENT_STORAGE_TOKEN`

For someone who operates the storage without controlling who may access it.

**Can:** buckets, objects, versions, lifecycle rules, quotas, storage inspection and
repair, integrity verification, share and embed links, cluster and node **reads**.

**Cannot:**

| Blocked | Why |
| --- | --- |
| `/api/v1/service-accounts` | Creating a credential is granting access |
| `/api/v1/policies` | Changing a policy is changing who can do what |
| `/api/v1/audit` | Reading the trail is a separate duty |
| `/api/v1/webhooks`, `/api/v1/webhook-deliveries` | A webhook target is a server-side fetch |
| Non-`GET` on `/api/v1/cluster`, `/api/v1/nodes`, `/api/v1/rebalance`, `/api/v1/repair` | Membership changes are system administration |

The cluster restriction is method-based: reads are allowed, mutations are not. A
storage administrator can see cluster state and cannot drain or decommission a node.

### Auditor

`RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN`

Read-only. `GET` requests only, to a fixed list of routes:

| Category | Routes |
| --- | --- |
| Identity | `/api/v1/auth/session`, `/api/v1/system/info` |
| Trails | `/api/v1/audit/events`, `/api/v1/events` |
| Storage | `/api/v1/storage/status`, `/api/v1/storage/usage`, `/api/v1/storage/inspect`, `/api/v1/buckets` |
| Webhooks | `/api/v1/webhooks`, `/api/v1/webhook-deliveries` |
| Cluster | `/api/v1/cluster*`, `/api/v1/nodes*`, `/api/v1/repair*`, `/api/v1/rebalance*` |
| Capabilities | Share and embed **metadata**, and `/api/v1/sharing/settings` |

!!! note "An auditor may not read a capability's URL"
    Share and embed metadata is readable — that a link exists, what it grants, when it
    expires. The `/url` routes are not.

    The URL *is* the capability. Handing it to a read-only role would be an escalation
    dressed up as a report.

An auditor cannot list objects in a bucket or read object content.

### Summary

| | System | Storage | Auditor |
| --- | --- | --- | --- |
| Buckets and objects | ✅ | ✅ | list buckets only |
| Lifecycle, quotas | ✅ | ✅ | ❌ |
| Storage repair, verification | ✅ | ✅ | read only |
| Service accounts | ✅ | ❌ | ❌ |
| Policies | ✅ | ❌ | ❌ |
| Webhooks | ✅ | ❌ | read only |
| Audit log | ✅ | ❌ | ✅ |
| Cluster mutation | ✅ | ❌ | ❌ |
| Cluster read | ✅ | ✅ | ✅ |
| Capability metadata | ✅ | ✅ | ✅ |
| Capability URLs | ✅ | ✅ | ❌ |

## Choosing a token

| Who | Token |
| --- | --- |
| The console, for full administration | system |
| A day-to-day storage operator | storage |
| Compliance, security review, monitoring dashboards | auditor |
| Automation that only reads state | auditor |
| CI that creates buckets and uploads | **a service account**, not a management token |

The last row matters. Management tokens are for administering the deployment. A build
that uploads artifacts wants an S3 credential with a narrow policy.

## Rotating a management token

There is no dual-token grace period. Rotation is a restart:

1. Generate a new token.
2. Update the configuration.
3. Restart the process.
4. Update every client.

Plan for the gap, or use a proxy that can present either during the transition.

## Verifying a token's role

```bash
curl https://management.example.com/api/v1/auth/session \
  -H "Authorization: Bearer <your-management-token>"
```

A `401` means the token is not recognised. A success tells you which role it carries —
which is what the console uses to decide what to offer.

## Auditing

Every management request is recorded with its role as the principal:
`management:system-administrator`, `management:storage-administrator`,
`management:auditor`, or `management:unauthenticated`.

```bash
record-store audit \
  --principal management:storage-administrator \
  --endpoint https://management.example.com
```

Because the principal is the role, not the person, share a role token no more widely
than the accountability you need. Two people with the same token are
indistinguishable in the trail.
