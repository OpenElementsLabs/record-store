# Health and Readiness

Two endpoints, both unauthenticated, both on the management port.

| Endpoint | Answers | Use for |
| --- | --- | --- |
| `/health` | Is the process alive? | Liveness probe |
| `/ready` | Can it serve requests? | Readiness probe, load balancer |

## `/health`

```bash
curl http://127.0.0.1:7601/health
```

```json
{"status": "ok"}
```

Always 200 if the process is running and accepting connections. It checks nothing else.

Use it for a liveness probe — where a failure should restart the process. Do **not** use
it to decide whether to send traffic; it answers a different question.

## `/ready`

```bash
curl http://127.0.0.1:7601/ready
```

```json
{"status": "ready"}
```

Returns `503` when any subsystem is not ready. It checks, concurrently:

- Object storage
- Metadata
- The audit store
- The event store, when webhooks are configured
- The sharing store, when sharing is configured

Use it for a readiness probe and for load-balancer health checks — where a failure
should stop traffic without restarting anything.

## `record-store status`

```bash
record-store status --endpoint https://management.example.com
```

Checks `/ready`, then prints system information if a management token is available:

```text
Ready              yes
Management API     https://management.example.com
Mode               standalone
```

The exit code is driven by readiness, so it works as a container healthcheck **with or
without** a token — which is exactly how the shipped Docker healthcheck uses it:

```dockerfile
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["record-store", "status", "--endpoint", "http://127.0.0.1:7601"]
```

Provide `RECORD_STORE_MANAGEMENT_TOKEN` when you want the version and capabilities too.

## System information

```bash
curl https://management.example.com/api/v1/system/info \
  -H "Authorization: Bearer <your-management-token>"
```

```json
{
  "name": "record-store",
  "version": "...",
  "status": "ready",
  "capabilities": {
    "versioning": true,
    "webhooks": true,
    "events": true,
    "lifecycle": true,
    "object_browser": true,
    "erasure_coding": false
  }
}
```

`capabilities` is what the deployment can actually do, resolved from its current
configuration — `webhooks` and `events`, for instance, reflect whether the event store
is configured.

`erasure_coding` is always `false`; no code path produces or reads erasure stripes. See
[Durability](../concepts/durability.md).

## Orchestrator probes

```yaml
livenessProbe:
  httpGet:
    path: /health
    port: 7601
  initialDelaySeconds: 10
  periodSeconds: 30

readinessProbe:
  httpGet:
    path: /ready
    port: 7601
  initialDelaySeconds: 5
  periodSeconds: 10
```

Wiring both to `/ready` is a common mistake: a transient storage problem then restarts
a process that would have recovered, and the restart makes it worse.

## When readiness fails

Look at the logs. The readiness failure is logged with the specific subsystem and
error:

```bash
docker compose logs record-store | grep "readiness check failed"
```

Common causes:

| Cause | Fix |
| --- | --- |
| Data directory not writable | Ownership — uid 10001 in the container image |
| Disk full | Free space; see [Capacity Planning](capacity-planning.md) |
| Metadata could not be opened | Another process holds the lock, or the directory is corrupt |
| Schema newer than the binary | You downgraded; see [Upgrading](../deployment/upgrading.md) |

## Graceful shutdown

On `SIGTERM` the server stops accepting new requests and drains in-flight ones within
`server.shutdown_grace_period_seconds` (default 30, range 1–300).

Give your orchestrator at least that long:

```bash
docker stop --time 40 record-store
```

Killing it sooner interrupts uploads in progress. It does not corrupt anything —
commits are atomic — but clients see failures they need not have seen.
