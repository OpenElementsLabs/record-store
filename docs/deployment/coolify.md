# Coolify

An end-to-end deployment of Record Store and the web console on a
[Coolify](https://coolify.io) server, with TLS on both public endpoints and the
management API kept private.

The repository ships `deploy/docker/docker-compose.yaml` for exactly this. It differs
from the development Compose files in three ways: it uses `expose` rather than
`ports`, it declares Coolify magic environment variables so Coolify generates and
retains the secrets, and it publishes only the two endpoints that should be public.

It builds from source. If you would rather have Coolify pull the
[published images](container-images.md) — faster deployments, and the same
artifact everywhere — replace each service's `build:` block with the pinned
image, keeping the rest of the file as it is:

```yaml
services:
  record-store:
    image: ghcr.io/openelementslabs/record-store:0.1.1
  console:
    image: ghcr.io/openelementslabs/record-store-console:0.1.1
```

Coolify needs registry credentials for that, because the packages are private
until a maintainer makes them public.

## What you need

- A Coolify server with a project and a server attached
- Two DNS records pointing at that server, for example
  `storage.example.com` and `console.example.com`
- This repository reachable from Coolify (a Git remote or a connected source)

## 1. Create the resource

In Coolify: **Project → New Resource → Docker Compose**, pointing at this repository.

Set the Compose file path to:

```text
deploy/docker/docker-compose.yaml
```

The build context is the repository root, so both images build from source in one
pass.

## 2. Understand the generated secrets

The Compose file uses Coolify's magic variables. Coolify generates each value on first
deploy and keeps it for the life of the resource:

| Variable | Becomes | Notes |
| --- | --- | --- |
| `SERVICE_USER_ROOTACCESS` | `RECORD_STORE_ROOT_ACCESS_KEY` | Root S3 access key |
| `SERVICE_PASSWORD_64_ROOTSECRET` | `RECORD_STORE_ROOT_SECRET_KEY` | 64 characters |
| `SERVICE_BASE64_64_CREDENTIALMASTER` | `RECORD_STORE_CREDENTIAL_MASTER_KEY` | 64 characters, satisfies the 32–1024 requirement |
| `SERVICE_PASSWORD_64_SYSTEMTOKEN` | `RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN` | Full management access |
| `SERVICE_PASSWORD_64_STORAGETOKEN` | `RECORD_STORE_MANAGEMENT_STORAGE_TOKEN` | Storage administration |
| `SERVICE_PASSWORD_64_AUDITORTOKEN` | `RECORD_STORE_MANAGEMENT_AUDITOR_TOKEN` | Read-only |
| `SERVICE_PASSWORD_64_SCRAPETOKEN` | `RECORD_STORE_METRICS_SCRAPE_TOKEN` | Prometheus only |

!!! danger "Back up the master key before you deploy anything real"
    `RECORD_STORE_CREDENTIAL_MASTER_KEY` cannot be rotated. If the Coolify resource is
    deleted and recreated, a **new** key is generated and every stored credential and
    encrypted object from the old deployment becomes unreadable. After the first
    deploy, copy it out of Coolify's environment view into your secret manager.

## 3. Assign domains

Two variables control the public URLs:

| Variable | Service | Set to |
| --- | --- | --- |
| `SERVICE_FQDN_RECORDSTORE_7600` | `record-store` | `https://storage.example.com` |
| `SERVICE_FQDN_CONSOLE_7602` | `console` | `https://console.example.com` |

Set them in Coolify's environment editor before the first deploy. Coolify configures
its proxy and provisions certificates for both.

Port `7601` has no FQDN variable and is only in `expose`, so it is reachable on the
internal Docker network and nowhere else. That is the intent — do not add a domain for
it.

## 4. Set the public base URLs

Share and embed links are absolute URLs, and Record Store cannot infer the public
hostname from behind a proxy. Add two more environment variables:

```bash
RECORD_STORE_SHARING_SHARE_BASE_URL=https://console.example.com
RECORD_STORE_SHARING_EMBED_BASE_URL=https://storage.example.com
```

They are different hosts on purpose: a share link is a page a person opens on the
console, and an embed serves object bytes from the storage endpoint. Skipping these
produces links that work on the server and nowhere else.

## 5. Deploy

Press **Deploy**. Coolify builds both images and starts the two services. The console
waits for the server's healthcheck.

The healthcheck allows a 45-second start period, which covers first-run initialization
of the data directory.

## 6. Verify

The S3 endpoint answers over TLS:

```bash
curl -i https://storage.example.com/
```

An unauthenticated request returning an S3 `AccessDenied` error is a correct response
— it proves the endpoint is reachable and signing is enforced.

Open `https://console.example.com` and sign in with the system token from Coolify's
environment view.

From Coolify's terminal for the `record-store` container:

```bash
record-store status --endpoint http://127.0.0.1:7601
```

## 7. Create a service account

Do not hand the root credential to applications. In the console, or from the
container's terminal:

```bash
RECORD_STORE_MANAGEMENT_TOKEN=<your-system-token> \
  record-store service-account create my-app \
  --endpoint http://127.0.0.1:7601
```

Attach a [policy](../administration/policies.md), then disable root S3 access by
adding to the environment:

```bash
RECORD_STORE_ROOT_S3_ENABLED=false
```

Redeploy for it to take effect.

## 8. Connect a client

```bash
aws configure set aws_access_key_id <service account access key>
aws configure set aws_secret_access_key <service account secret key>
aws configure set region us-east-1

aws --endpoint-url https://storage.example.com s3 mb s3://uploads
aws --endpoint-url https://storage.example.com s3 ls
```

See [AWS CLI](../guides/aws-cli.md).

## Persistence

The Compose file declares a `record-store-data` named volume mounted at
`/var/lib/record-store`. It survives redeploys and restarts, and is deleted if you
delete the resource with its volumes.

Coolify's own backups do not know how to quiesce Record Store's metadata. Take
metadata backups with the CLI as well:

```bash
record-store server backup-metadata --output /var/lib/record-store/backup
```

See [Backup and Restore](../operations/backup-and-restore.md) and
[Persistent Storage](persistent-storage.md).

## Uploads through the proxy

Coolify's proxy applies a request body size limit. If large uploads fail with a
proxy-generated `413`, raise the limit for the storage domain or have clients use
[multipart uploads](../guides/multipart-uploads.md), which send bounded parts.

## Updating

Push to the tracked branch and redeploy. Coolify rebuilds the images and recreates the
containers; the data volume is untouched. Read [Upgrading](upgrading.md) first.

If you switched to the published images, updating means changing the version tag
and redeploying — nothing is rebuilt, and the version you get is the version you
named.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| Container never becomes healthy | Missing or invalid required environment. Read the container logs — validation reports every problem at once |
| Console shows a connection error | `RECORD_STORE_API_URL` must be `http://record-store:7601`, the internal service name |
| Signed in, then immediately signed out | `RECORD_STORE_CONSOLE_SECURE_COOKIES` must be `true` behind TLS |
| Share or embed links point at `127.0.0.1` | The two base URLs are not set — see step 4 |
| Uploads fail at a size threshold | Proxy body limit |

More in [Docker and Coolify](../troubleshooting/docker-and-coolify.md).
