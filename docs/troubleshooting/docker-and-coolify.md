# Docker and Coolify

## Container never becomes healthy

Read the logs first. Configuration validation reports **every** problem at once, so one
look tells you all of them.

```bash
docker logs record-store
docker compose logs record-store
```

Common causes:

| Log message | Cause |
| --- | --- |
| `root credentials are required` | `RECORD_STORE_ROOT_ACCESS_KEY` / `_SECRET_KEY` not set |
| `credential_master_key is required` | Encryption on without a master key |
| `must contain 32 to 1024 visible ASCII characters` | A token is too short |
| `management role tokens must be distinct` | Two role tokens are the same value |
| `metrics_scrape_token must be distinct` | It matches a role token |
| `data directory in use` | Another container holds the lock |

Check the healthcheck directly:

```bash
docker inspect --format '{{.State.Health.Status}}' record-store
docker exec record-store record-store status --endpoint http://127.0.0.1:7601
```

## Permission denied on the data directory

The image runs as uid `10001`. A bind-mounted host directory must be writable by it:

```bash
sudo install -d -o 10001 -g 10001 /srv/record-store
```

Named volumes do not have this problem — Docker sets the ownership from the image.

## `docker exec` says the token is required

`exec` starts a fresh process that does not inherit the service's environment:

```bash
docker exec \
  -e RECORD_STORE_MANAGEMENT_TOKEN=<your-management-token> \
  record-store \
  record-store bucket list --endpoint http://127.0.0.1:7601
```

The `-e` is not optional.

## Data disappears on restart

No volume is mounted, so the data directory lives in the container's writable layer and
goes with it.

```yaml
volumes:
  - record-store-data:/var/lib/record-store
```

Check:

```bash
docker inspect record-store --format '{{json .Mounts}}' | jq
```

`docker compose down -v` deletes named volumes. `down` alone keeps them.

## Console cannot reach the server

```yaml
environment:
  RECORD_STORE_API_URL: "http://record-store:7601"
```

The **Compose service name**, not `localhost` — inside the console container,
`localhost` is the console.

Check from inside:

```bash
docker compose exec console wget -qO- http://record-store:7601/health
```

## Console signs you straight out

```yaml
RECORD_STORE_CONSOLE_SECURE_COOKIES: "true"
```

Required behind TLS. The shipped development Compose files set `false` because local
development is plain HTTP on loopback.

## Read-only filesystem errors

The provided Compose files run with `read_only: true`, which works because everything
mutable is on the data volume or in `tmpfs`:

```yaml
read_only: true
tmpfs:
  - /tmp
```

If something needs another writable path, add it to `tmpfs` rather than removing
`read_only`.

## Container is killed during shutdown

The server drains for up to `server.shutdown_grace_period_seconds` (default 30). Give
Docker longer than that:

```bash
docker stop --time 40 record-store
```

```yaml
stop_grace_period: 40s
```

## Coolify: which Compose file

```text
deploy/docker/docker-compose.yaml
```

That one uses `expose` rather than `ports` and declares Coolify's magic variables. The
`compose*.yml` files in the same directory are for local development and publish ports
directly.

## Coolify: no TLS, or the wrong domain

Two variables control the public URLs:

| Variable | Service |
| --- | --- |
| `SERVICE_FQDN_RECORDSTORE_7600` | `record-store` |
| `SERVICE_FQDN_CONSOLE_7602` | `console` |

Set both in Coolify's environment editor before the first deploy. Port 7601 has no FQDN
variable on purpose — it must stay private. Do not add one.

## Coolify: share and embed links are wrong

Add:

```bash
RECORD_STORE_SHARING_SHARE_BASE_URL=https://console.example.com
RECORD_STORE_SHARING_EMBED_BASE_URL=https://storage.example.com
```

Record Store cannot infer its public hostname from behind Coolify's proxy.

## Coolify: uploads fail at a size threshold

Coolify's proxy applies a body-size limit. Raise it for the storage domain, or have
clients use [multipart uploads](../guides/multipart-uploads.md).

## Coolify: everything broke after recreating the resource

Coolify generates a **new** `SERVICE_BASE64_64_CREDENTIALMASTER` for a new resource.
The master key cannot be rotated: every credential and every encrypted object from the
old deployment is now unreadable.

If you still have the old key, set `RECORD_STORE_CREDENTIAL_MASTER_KEY` explicitly to
it.

If you do not, the data is gone. This is why the
[Coolify guide](../deployment/coolify.md) says to copy the master key out of Coolify
into your secret manager after the first deploy.

## Slow builds

The Dockerfile builds from source with `--locked --release`. That is a full compile.

Build once and reuse the image rather than rebuilding on every deploy. Coolify caches
layers, so an unchanged dependency set is much faster on subsequent builds.

## Reading logs

```bash
docker compose logs -f record-store
docker compose logs record-store | grep '"level":"ERROR"'
```

The image sets `RECORD_STORE_LOG_JSON=true`. For more detail:

```bash
RECORD_STORE_LOG=record_store=debug
```

That is read at startup, so it needs a restart.
