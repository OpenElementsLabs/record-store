# Health and Readiness

Three questions, asked in three different places, because they have three different
answers.

| Where | Answers | Use for |
| --- | --- | --- |
| `record-store server doctor` | Can this machine run the deployment at all? | Before starting, and when it will not start |
| `/health` | Is the process alive? | Liveness probe |
| `/ready` | Can it serve requests? | Readiness probe, load balancer |

Conflating the last two is the common mistake; conflating any of them with the first
is why a deployment that was never going to work spends a minute starting before it
says so.

## `record-store server doctor`

```bash
record-store server --config /etc/record-store/config.toml doctor
```

```text
ok    configuration                 every configured value is within range
ok    data_directory                /var/lib/record-store exists and is writable
ok    atomic_publication            .../tmp and .../objects are on one filesystem, so payloads publish with a rename
ok    storage_format                on-disk storage format 1
ok    restore_state                 no interrupted restore
ok    free_space                    62% of the data filesystem is free (74724573184 bytes)
ok    s3_listener                   0.0.0.0:7600 is free
ok    management_listener           0.0.0.0:7601 is free
ok    credential_master_key         a credential master key is configured
warn  metrics_token                 no metrics scrape token is configured; the metrics endpoint stays closed
                                    -> set RECORD_STORE_METRICS_SCRAPE_TOKEN if you intend to scrape metrics
```

It starts nothing, opens no database, and never prints a secret value — checks about
key material report only whether a key is present and whether it matches what is
already on disk. Every failure carries the corrective action.

It exits 0 when nothing failed and 7 when something did. `--json` emits the same
report for automation.

The checks that would otherwise be discovered halfway through start-up — an unwritable
data directory, a temporary directory on the wrong filesystem, an unfinished restore —
also run automatically when the server starts, before anything durable is opened. The
listeners are bound before initialization for the same reason, so an occupied address
fails immediately rather than after every subsystem is already running.

`record-store server check-config` remains for the narrower question of whether the
configuration *values* are valid, with no reference to the machine.

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
