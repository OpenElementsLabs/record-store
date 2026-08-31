# Docker Compose

Three Compose files ship in `deploy/docker/`:

| File | What it runs | Image |
| --- | --- | --- |
| `compose.ghcr.yml` | Record Store plus the web console | Published |
| `compose.yml` | Record Store on its own | Built from source |
| `compose.console.yml` | Record Store plus the web console | Built from source |

The two source-building files are development configurations. Every credential
in them is a `change-me` placeholder that exists so `docker compose up` works with
no setup. **Override every one before running anything real.**

`compose.ghcr.yml` is the one to deploy. It pulls the
[published images](container-images.md), builds nothing, and refuses to start
until every secret is set:

```bash
RECORD_STORE_VERSION=0.1.1 \
  docker compose --env-file .env -f deploy/docker/compose.ghcr.yml up -d
```

## Server only

```bash
cd deploy/docker
docker compose -f compose.yml up -d
```

Published ports: `7600` (S3) and `7601` (management). Data lives in the
`record-store-data` named volume.

### Overriding the credentials

Create `deploy/docker/.env`:

```bash
RECORD_STORE_ROOT_ACCESS_KEY=<your-access-key>
RECORD_STORE_ROOT_SECRET_KEY=<your-secret-key>
RECORD_STORE_CREDENTIAL_MASTER_KEY=<your-master-key>
RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN=<your-system-token>
RECORD_STORE_MANAGEMENT_STORAGE_TOKEN=<your-storage-token>
RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN=<your-auditor-token>
RECORD_STORE_METRICS_SCRAPE_TOKEN=<your-metrics-token>
```

Generate them:

```bash
openssl rand -base64 48
```

The three management tokens must be distinct, and the metrics token must differ from
all of them. Add `.env` to `.gitignore`.

For production also restrict the management port to loopback:

```yaml
ports:
  - "7600:7600"
  - "127.0.0.1:7601:7601"
```

## With the console

```bash
docker compose -f compose.console.yml up -d
```

This adds a `console` service on `7602`. Two things about it are worth understanding:

- `RECORD_STORE_API_URL` is `http://record-store:7601` — the **Compose network name**.
  The console server calls the management API; the browser never does. The management
  port does not need to be reachable from the browser at all.
- `RECORD_STORE_CONSOLE_SECURE_COOKIES` defaults to `false` here because local
  development is plain HTTP on loopback. **Set it to `true` behind TLS** so the
  session cookie is marked `Secure`.

The console waits for the server's healthcheck before starting.

Open <http://localhost:7602> and sign in with a management token.

## Running commands

```bash
docker compose -f compose.yml exec \
  -e RECORD_STORE_MANAGEMENT_TOKEN=<your-system-token> \
  record-store \
  record-store bucket list --endpoint http://127.0.0.1:7601
```

The `-e` is required — `exec` starts a fresh process that does not inherit the
service's environment.

## Logs and shutdown

```bash
docker compose -f compose.yml logs -f record-store
docker compose -f compose.yml down
```

`down` keeps named volumes. `down -v` deletes them, and with them all your data.

## Moving toward production

- Override every credential.
- Bind `7601` to loopback.
- Put a TLS terminator in front of `7600` and `7602` — see
  [Reverse Proxy and TLS](reverse-proxy.md).
- Set `RECORD_STORE_CONSOLE_SECURE_COOKIES=true`.
- Set `RECORD_STORE_SHARING_EMBED_BASE_URL` and `RECORD_STORE_SHARING_SHARE_BASE_URL`
  to the public hostnames.
- Replace the named volume with storage you back up — see
  [Persistent Storage](persistent-storage.md).
- Work through the [Production Checklist](production-checklist.md).
