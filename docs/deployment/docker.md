# Running with Docker

Starting Record Store as a container by hand: what the image expects, how to run it,
and how to keep it healthy. [Container Images](container-images.md) covers the other
half — *which* image to pull, its tags, and how to pin one; this chapter is about
what happens once you have it.

Everything here uses `docker run` directly. For anything longer-lived than a look
around, use [Docker Compose](docker-compose.md) instead.

## The image

```bash
docker pull ghcr.io/openelementslabs/record-store:0.1.1
```

It is built from the multi-stage Dockerfile at `deploy/docker/Dockerfile`. Building
it yourself is only needed for development, or for a change you have not released:

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
| Ports served | `7600`, `7601` |
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
  ghcr.io/openelementslabs/record-store:0.1.1
```

Note the asymmetry in the port publishing: `7600` is bound on all interfaces, `7601`
only on loopback. That is deliberate — see [Ports](../reference/ports.md).

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

The web console is a separate image, built from
`deploy/docker/Dockerfile.console`:

```bash
docker pull ghcr.io/openelementslabs/record-store-console:0.1.1
```

Or build it:

```bash
docker build -t record-store-console:local -f deploy/docker/Dockerfile.console .
```

It is deliberately separate so a headless deployment carries no frontend and the
console can be upgraded on its own schedule. It is a client of the management
API and holds no state of its own.

### What the console image does

| | |
| --- | --- |
| Base | `node:24-bookworm-slim` |
| User | non-root, uid/gid `10002` |
| Listens on | `7602` |
| Command | `node server.js` (Next.js standalone) |
| Stop signal | `SIGTERM` |
| Healthcheck | `GET http://127.0.0.1:7602/login` |

It reads its configuration at runtime, not at build time, so one image works for
any deployment:

| Variable | Default | Meaning |
| --- | --- | --- |
| `RECORD_STORE_API_URL` | `http://record-store:7601` | Where the management API lives, reached from the **container**, not the browser |
| `RECORD_STORE_CONSOLE_SECURE_COOKIES` | `true` in production | Whether the session cookie is marked `Secure` |
| `PORT` | `7602` | Port to listen on |

### Running the console

The console has to reach the management API, and `127.0.0.1` inside the console
container is the console itself — not the server. Put both containers on a
user-defined network and address the server by its container name:

```bash
docker network create record-store
```

Run the server on that network, adding `--network record-store` to the
[`docker run` above](#running). Then:

```bash
docker run -d \
  --name record-store-console \
  --network record-store \
  --read-only \
  --tmpfs /tmp \
  --security-opt no-new-privileges:true \
  -p 7602:7602 \
  -e RECORD_STORE_API_URL=http://record-store:7601 \
  ghcr.io/openelementslabs/record-store-console:0.1.1
```

Then open <http://localhost:7602> and sign in with a management token.

!!! warning "Signing in over plain HTTP"
    The session cookie is marked `Secure` by default, and a browser will not
    store a `Secure` cookie sent over `http://` — except on `localhost`, which is
    exempt. So the command above works on your own machine, but the same setup
    reached at `http://a-server:7602` accepts the token and then behaves as
    though you never signed in.

    Put TLS in front of it, which is the right answer anyway — see
    [Reverse Proxy and TLS](reverse-proxy.md). Only if you genuinely cannot,
    and the network is trusted, set `RECORD_STORE_CONSOLE_SECURE_COOKIES=false`.

Check it came up:

```bash
docker inspect --format '{{.State.Health.Status}}' record-store-console
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7602/login
```

### What to expose

Publish `7602`. Do **not** publish `7601` to reach the console — the console
talks to the management API over the Docker network, and the management API is
unrestricted administrative access that must not face the internet. See
[Ports](../reference/ports.md).

### Two containers, one command

Running both by hand is fine for a look around. For anything longer-lived use
Compose, which handles the network, ordering, and health gating for you — see
[Docker Compose](docker-compose.md).
