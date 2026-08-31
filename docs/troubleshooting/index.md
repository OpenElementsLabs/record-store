# Troubleshooting

<div class="grid cards" markdown>

-   **[Authentication Errors](authentication.md)** — 401, 403, signature failures
-   **[Upload Problems](uploads.md)** — failed, truncated, or refused uploads
-   **[Networking and Proxies](networking.md)** — proxy, TLS, and CORS problems
-   **[Docker and Coolify](docker-and-coolify.md)** — container-specific issues
-   **[FAQ](faq.md)** — common questions

</div>

## Start here

```bash
# Is it ready?
record-store status --endpoint https://management.example.com

# What did it decide, and why?
record-store audit --limit 20 --endpoint https://management.example.com

# What is on disk?
record-store storage inspect --endpoint https://management.example.com
```

## Use the request ID

Every response carries one — `x-request-id` on the management API,
`x-amz-request-id` on the S3 API. The same ID appears on the log line and the audit
event:

```bash
curl -G https://management.example.com/api/v1/audit/events \
  -H "Authorization: Bearer <your-management-token>" \
  --data-urlencode "request_id=<request id from the response header>"
```

That is the fastest path from "a user saw an error" to "here is what the server
decided". Ask for it whenever someone reports a failure.

## Turn up the logs

```bash
RECORD_STORE_LOG=record_store=debug
```

Read at startup, so this needs a restart. Narrow it to the relevant crate rather than
debugging everything:

| Area | Filter |
| --- | --- |
| S3 requests | `record_store=info,record_store_s3=debug` |
| Storage | `record_store=info,record_store_storage=debug` |
| Webhooks | `record_store=info,record_store_events=debug` |
| Sharing | `record_store=info,record_store_sharing=debug` |

## Frequent causes

| Symptom | Usually |
| --- | --- |
| `SignatureDoesNotMatch` | A proxy rewriting the `Host` header |
| `NotImplemented` on upload | The SDK is sending `aws-chunked` checksums |
| Uploads fail above a size | A proxy body-size limit |
| Console signs you straight out | `RECORD_STORE_CONSOLE_SECURE_COOKIES` not set behind TLS |
| Share or embed links point at `127.0.0.1` | The two base URLs are not set |
| `403` from a working key | The policy does not cover the action or resource |
