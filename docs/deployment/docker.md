# Docker

The repository ships a multi-stage Dockerfile at
`deploy/docker/Dockerfile` that produces a small runtime image.

## Building

```bash
docker build -t record-store:local -f deploy/docker/Dockerfile .
```

The build compiles with `--locked --release` and produces both binaries:
`record-store-server` (the daemon) and `record-store` (the CLI). Having the CLI in the
image is what lets the healthcheck and `docker exec` administration work without a
second image.

## What the image does

| | |
| --- | --- |
| Base | `debian:bookworm-slim` |
| User | non-root, uid/gid `10001` |
| Data volume | `/var/lib/record-store` |
| Exposed | `7600`, `7601`, `7603` |
| Entrypoint | `record-store server` |
| Stop signal | `SIGTERM` |
| Healthcheck | `record-store status --endpoint http://127.0.0.1:7601` |

Built-in environment defaults:

```bash
RECORD_STORE_S3_BIND=0.0.0.0:7600
RECORD_STORE_API_BIND=0.0.0.0:7601
RECORD_STORE_STORAGE_DATA_DIRECTORY=/var/lib/record-store
RECORD_STORE_LOG_JSON=true
```

JSON logging is on by default because a container's logs are almost always collected
by something that parses them.

## Running

```bash
docker run -d \
  --name record-store \
  -p 7600:7600 \
  -p 127.0.0.1:7601:7601 \
  -v record-store-data:/var/lib/record-store \
  -e RECORD_STORE_ROOT_ACCESS_KEY=<your-access-key> \
  -e RECORD_STORE_ROOT_SECRET_KEY=<your-secret-key> \
  -e RECORD_STORE_CREDENTIAL_MASTER_KEY=<your-master-key> \
  -e RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=<your-system-token> \
  record-store:local
```

Note the asymmetry in the port publishing: `7600` is bound on all interfaces, `7601`
only on loopback. That is deliberate — see [Ports](../reference/ports.md).

Do not publish `7603` at all outside a cluster.

## Secrets

Passing secrets with `-e` puts them in `docker inspect` output and in your shell
history. For anything beyond a local experiment use an env file with restricted
permissions, or your orchestrator's secret mechanism:

```bash
docker run -d --env-file /etc/record-store/env ...
```

Never bake secrets into the image.

## Healthcheck

```bash
record-store status --endpoint http://127.0.0.1:7601
```

The command checks `/ready` and exits non-zero if the server is not ready. It also
prints system information when a management token is present in the environment; with
no token it still exits 0, which is exactly what a healthcheck needs.

Check it:

```bash
docker inspect --format '{{.State.Health.Status}}' record-store
```

## Hardening

The provided Compose files run the container with:

```yaml
read_only: true
tmpfs:
  - /tmp
security_opt:
  - no-new-privileges:true
```

A read-only root filesystem works because everything mutable lives on the data volume
and in `/tmp`. Carry these over to whatever runs your container.

## Administration from the container

```bash
docker exec \
  -e RECORD_STORE_MANAGEMENT_TOKEN=<your-system-token> \
  record-store \
  record-store bucket list --endpoint http://127.0.0.1:7601
```

The `-e` is required: `docker exec` does not inherit the container's environment for
the new process, so without it the CLI has no credential.

## Stopping

The container stops on `SIGTERM` and drains in-flight requests within
`server.shutdown_grace_period_seconds` (default 30). Give Docker at least that long:

```bash
docker stop --time 40 record-store
```

## Console image

The web console is a separate image, `deploy/docker/Dockerfile.console`:

```bash
docker build -t record-store-console:local -f deploy/docker/Dockerfile.console .
```

It is deliberately separate so a headless deployment carries no frontend and the
console can be upgraded on its own schedule. It listens on `7602` and reads
`RECORD_STORE_API_URL` at runtime, so one image works for any deployment.

Running the two together is easiest with Compose — see
[Docker Compose](docker-compose.md).
