# Audit Log

The audit log is a durable, append-only record of who did what. It is stored
separately from process logs on purpose: a log file rotates away, and a security
question asked six months later still needs an answer.

Audit records never contain secrets — no keys, no tokens, no passwords.

## What is recorded

| Source | Principal | Operation | Resource |
| --- | --- | --- | --- |
| S3 API request | `service_account:<id>`, `system:<component>`, or `anonymous` | `s3:GET`, `s3:PUT`, `s3:DELETE`, … | `bucket:<name>` or `bucket:<name>/<key>` |
| Management API request | `management:system-administrator`, `management:storage-administrator`, `management:auditor`, or `management:unauthenticated` | `<METHOD> <route>` | the route |
| Share and embed administration | the management role | `share.created`, `share.revoked`, `share.deleted`, `embed.created`, `embed.updated`, `embed.revoked`, `embed.deleted` | the capability's target |
| Public capability refusal | `capability:public` | `share.denied`, `share.password_failed`, `share.password_throttled`, `embed.denied` | the capability's target |
| Lifecycle expiry | `system:lifecycle` | `lifecycle.expire-object`, `lifecycle.expire-noncurrent-version` | `bucket:<name>/<key>` |

Every event carries an event ID, a timestamp, the result, and — where one exists — the
request ID and source address.

### Results

| Result | Meaning |
| --- | --- |
| `attempted` | an authorized change is about to be made; its outcome is not known yet |
| `success` | 2xx |
| `denied` | 401 or 403 |
| `failure` | anything else |

`denied` is the one to alert on. A burst of denials from one principal is either a
misconfigured client or someone probing.

### Why a change leaves two records

A record written after a change commits can be lost — the process can die in the gap,
and the write itself can fail. So a request that changes something writes twice:

1. an `attempted` record, **before** the change, naming who asked and for what;
2. a second record with the real result, carrying the first one's identifier in
   `metadata.intent_event_id`.

Reads keep their single record. What the pair buys is that the three states are
distinguishable:

| In the log | What it means |
| --- | --- |
| `attempted` and an outcome | the ordinary case |
| `attempted` alone | the server cannot account for this operation — it may have committed |
| an outcome alone | impossible, and evidence the log was edited |

An `attempted` record with no outcome is deliberately **not** resolved afterwards.
The server has no way to know what happened either, and writing a guess would turn an
honest gap into a false record. Query for them directly:

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "result=attempted"
```

!!! warning "A change that cannot be recorded is refused"
    If the `attempted` record cannot be made durable — a full disk, a corrupt audit
    database — the request fails with `503` and nothing is changed. A change nobody
    can account for afterwards is worse than a change that did not happen. The audit
    store is also part of the [readiness probe](../operations/health-and-readiness.md),
    so a node in this state reports itself unready.

!!! note "Successful public accesses are counted, not audited"
    A share or embed that serves a video answers thousands of range requests. Writing
    an immutable audit row for each would let an anonymous visitor fill the security
    trail — that is the vulnerability, not the protection. Successful public accesses
    increment [metrics](metrics.md); refusals are audited in full.

## Reading it

```bash
record-store audit --endpoint https://management.example.com
```

Filters:

```bash
record-store audit \
  --principal service_account:<account-id> \
  --operation "s3:DELETE" \
  --limit 200 \
  --endpoint https://management.example.com
```

The API accepts more filters than the CLI exposes:

| Parameter | Matching |
| --- | --- |
| `since`, `until` | RFC 3339 timestamps bounding the scan |
| `principal` | exact |
| `operation` | exact |
| `resource` | prefix |
| `result` | `success`, `denied`, or `failure` |
| `source_ip` | exact |
| `request_id` | exact |
| `limit` | 1–1000, default 100 |

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "since=2026-08-01T00:00:00Z" \
  --data-urlencode "result=denied" \
  --data-urlencode "limit=500"
```

## How filtering performs

Filters narrow a **bounded scan over the time range** — they do not consult an index.
Every filter costs the same: one comparison per scanned event.

The practical consequence: narrow the time range, not the filter. `since` and `until`
are the only parameters that make a query cheaper.

## Pagination

The response carries `next_time` and `next_id` when more results exist. Pass both back
as `after_time` and `after_id`:

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "after_time=2026-08-14T09:12:44.129Z" \
  --data-urlencode "after_id=<next_id from the previous page>"
```

Both cursor fields are required together. Sending one without the other is a
`400 INVALID_AUDIT_CURSOR`.

### Short pages are not the end of the range

Because a filter is a scan rather than an index lookup, the server gives up after a
bounded number of scanned records. When that happens the response sets:

```json
{ "events": [], "next_time": "...", "next_id": "...", "scan_truncated": true }
```

`scan_truncated: true` means **the range was not exhausted** — the page can be short,
or empty, while matches remain further on. Follow the cursor. Treating an empty page
as "nothing more" would hide activity, which is the one direction that matters.

A sparse filter over a long range is where this bites. Narrowing `since` and `until`
avoids it entirely.

## Checking the log has not been edited

Every record is linked to the one before it by a SHA-256 hash chain, and the positions
are gapless, so an edited, removed, or reordered record is detectable:

```bash
record-store audit-export verify-chain --endpoint https://management.example.com
```

```json
{ "intact": true, "from": 0, "checked": 41207, "verified": 41207,
  "unchained": 0, "next_from": null, "head_sequence": 41206, "problems": [] }
```

A long log is walked in spans: follow `next_from` until it is absent.

| Field | Meaning |
| --- | --- |
| `intact` | every record examined in this span verified |
| `verified` | records that hash to what they claim and link to their predecessor |
| `unchained` | records written before this release, which carry no links and are not evidence |
| `problems[].verdict` | `content changed`, `broken link to the previous record`, or `record is missing from the log` |

!!! note "What the chain does and does not establish"
    It detects a record edited, removed, or reordered by anyone who could not also
    rewrite every later link — which is what someone with access to the database file
    would have to do.

    It does **not** detect an operator who rewrote the whole log and every hash in it.
    Only an external anchor over a checkpoint reaches that far, and this release does
    not produce one. Records written by an earlier release carry no links at all and
    are reported as `unchained` rather than counted as verified.

    The format is specified in [Audit Chain and Checkpoints](../reference/audit-chain.md)
    so a verifier can be built from it without reading Record Store's source.

## Tracing one operation end to end

Every response carries a request ID header, and it appears on both the audit event and
the structured log line. Given an ID from a client error report:

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "request_id=<request id from the response header>"
```

That is the fastest path from "a user saw an error" to "here is exactly what the
server decided".

## Access

Reading the audit log requires a management token. The auditor role is the right one
for anybody whose job is to read it and nothing else — it is read-only and cannot
change configuration or data. See [Authorization](../security/authorization.md).

## Retention

The audit store grows without bound; nothing prunes it. Size it in
[Capacity Planning](../operations/capacity-planning.md) and include it in
[Backup and Restore](../operations/backup-and-restore.md).
