# Events and Webhooks

Storage events record changes to buckets and objects. They can be read from the
management API or delivered to an HTTP endpoint as webhooks.

Events are about **data**. For "who changed a setting", see the
[Audit Log](audit-log.md).

## Event types

| Type | Emitted when |
| --- | --- |
| `bucket.created` | A bucket is created |
| `bucket.deleted` | A bucket is deleted |
| `object.created` | An object is written where none existed |
| `object.updated` | An existing object is overwritten |
| `object.deleted` | An object or version is deleted |
| `object.restored` | A previous version is restored as the current one |
| `multipart.completed` | A multipart upload is completed |
| `multipart.aborted` | A multipart upload is aborted |

## Event payload

```json
{
  "id": "0198e3c1-6d2a-7b41-9f30-4c8ad2f9e611",
  "type": "object.created",
  "time": "2026-08-14T09:12:44.129Z",
  "bucket": "uploads",
  "object": "invoices/2026/03/inv-1.pdf",
  "version_id": "0198e3c1-6d2a-7b41-9f30-4c8ad2f9e612",
  "size": 184320,
  "metadata": {}
}
```

`object`, `version_id`, and `size` are absent for bucket-level events.

## Reading events

```bash
curl -G https://management.example.com/api/v1/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "bucket=uploads" \
  --data-urlencode "type=object.created" \
  --data-urlencode "limit=100"
```

Filters: `since`, `until`, `bucket`, `type`, `prefix`, and `limit`. Pagination uses
`after_time` and `after_id` together, taken from the previous page's `next_time` and
`next_id`. Sending one without the other is a `400 INVALID_EVENT_CURSOR`.

## Webhooks

### Creating one

```bash
record-store webhook create ./webhook.json \
  --endpoint https://management.example.com
```

```json
{
  "target_url": "https://hooks.example.com/record-store",
  "event_types": ["object.created", "object.deleted"],
  "bucket_filter": "uploads",
  "object_prefix_filter": "invoices/",
  "enabled": true
}
```

| Field | Required | Effect |
| --- | --- | --- |
| `target_url` | yes | Where deliveries are posted |
| `event_types` | yes | Which events to deliver |
| `bucket_filter` | no | Restrict to one bucket |
| `object_prefix_filter` | no | Restrict to one key prefix |
| `enabled` | no | Defaults to `true` |

The response contains the subscription **and its signing secret**:

```json
{
  "subscription": { "id": "…", "target_url": "…", "enabled": true },
  "signing_secret": "<shown once>"
}
```

!!! warning "The signing secret is shown once"
    Record Store stores it encrypted under the credential master key and never returns
    it again. Store it with your receiver's configuration at creation time. If it is
    lost, delete the webhook and create a new one.

### Delivery request

Each delivery is a `POST` with the event as the JSON body:

| Header | Value |
| --- | --- |
| `content-type` | `application/json` |
| `x-record-store-event-id` | The event's unique ID |
| `x-record-store-event-type` | e.g. `object.created` |
| `x-record-store-event-time` | RFC 3339 timestamp |
| `x-record-store-signature` | `sha256=<hex HMAC-SHA256 of the raw body>` |

Redirects are not followed. A `3xx` response counts as a failure.

### Verifying the signature

Compute HMAC-SHA256 over the **raw request body** with the signing secret and compare
in constant time.

```javascript
import { createHmac, timingSafeEqual } from "node:crypto";

export function verify(rawBody, header, secret) {
  const expected = "sha256=" + createHmac("sha256", secret).update(rawBody).digest("hex");
  const a = Buffer.from(expected);
  const b = Buffer.from(header ?? "");
  return a.length === b.length && timingSafeEqual(a, b);
}
```

```python
import hmac
from hashlib import sha256

def verify(raw_body: bytes, header: str, secret: str) -> bool:
    expected = "sha256=" + hmac.new(secret.encode(), raw_body, sha256).hexdigest()
    return hmac.compare_digest(expected, header or "")
```

Verify before parsing, and use the exact bytes received — re-serializing the JSON
changes the signature.

### Retries

A delivery succeeds on any 2xx. Otherwise it is retried with exponential backoff:
roughly `2^attempt` seconds plus a small deterministic jitter, up to
`webhooks.maximum_attempts` (default 6). After that the delivery is permanently
failed and is not retried.

At-least-once delivery is the guarantee. **Make your receiver idempotent** — key on
`x-record-store-event-id`, which is stable across retries of the same event.

### Delivery log

```bash
record-store webhook deliveries --limit 50 \
  --endpoint https://management.example.com
```

Each entry records the attempt number, HTTP status, timestamp, success flag, and a
bounded error summary. Response bodies are never stored — an arbitrary remote body is
not something to keep in the database.

### Enabling, disabling, deleting

```bash
curl -X PUT https://management.example.com/api/v1/webhooks/<webhook-id>/status \
  -H "Authorization: Bearer <your-management-token>" \
  -H "Content-Type: application/json" \
  -d '{"enabled":false}'

curl -X DELETE https://management.example.com/api/v1/webhooks/<webhook-id> \
  -H "Authorization: Bearer <your-management-token>"
```

A disabled webhook drops its pending deliveries rather than queueing them for later.

## Target restrictions

A webhook URL is supplied by an administrator and fetched by the server, which makes
it a server-side request forgery risk. Record Store applies these checks on **every**
delivery attempt, not only at creation:

- HTTPS only, unless `webhooks.allow_http` is on.
- No credentials in the URL, and no fragment.
- The hostname is resolved and **every** resolved address must be public, unless
  `webhooks.allow_private_networks` is on.
- The connection goes to the addresses that were validated, so a DNS answer cannot
  change between the check and the request.
- Redirects are not followed.

```toml
[webhooks]
allow_http = false
allow_private_networks = false
request_timeout_seconds = 10
maximum_attempts = 6
poll_interval_seconds = 2
```

Turning either flag on lets an administrator aim deliveries at loopback and internal
services. Do that only for development or a deliberately internal receiver, and treat
webhook creation as a privileged operation when you do.

## Receiver checklist

- Verify the signature before parsing.
- Respond 2xx quickly; queue the work rather than doing it inline. Slow responses hit
  `request_timeout_seconds` and become retries.
- Deduplicate on `x-record-store-event-id`.
- Do not assume ordering. Retries interleave with new deliveries; use `time` and
  `version_id` if order matters.
