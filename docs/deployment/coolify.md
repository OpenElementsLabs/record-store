# Coolify

An end-to-end deployment of Record Store and the web console on a
[Coolify](https://coolify.io) server, with TLS on both public endpoints and the
management API kept private.

The repository ships two Compose files for exactly this:

| File | Images | Use it when |
| --- | --- | --- |
| `deploy/docker/docker-compose.ghcr.yaml` | [Published](container-images.md), pinned | Normal deployments |
| `deploy/docker/docker-compose.yaml` | Built from source on the server | Deploying an unreleased change from a fork |

Prefer the first. Nothing is compiled on the server, deployments take seconds
rather than minutes, and the artifact you run is the one that was released and
tested. Both packages are public, so Coolify pulls them with no registry
credentials configured. Set `RECORD_STORE_VERSION` to the release you intend to
run.

Both differ from the `compose.*` development files in three ways: they use
`expose` rather than `ports`, they declare Coolify magic environment variables so
Coolify generates and retains the secrets, and they publish only the two
endpoints that should be public.

!!! warning "`compose.ghcr.yml` is not the Coolify file"
    Despite the name, `deploy/docker/compose.ghcr.yml` cannot be deployed here. It
    publishes host ports straight past Coolify's proxy, declares no
    `SERVICE_FQDN_*` variables — so Coolify has no domain to detect or show — and
    its `:?` required variables fail the deploy before Coolify can generate any
    secret. It is for a plain `docker compose up` on a host you control. The file
    with the `docker-compose.` prefix is the Coolify one.

## What you need

- A Coolify server with a project and a server attached
- Two DNS records pointing at that server, for example
  `storage.example.com` and `console.example.com`
- This repository reachable from Coolify (a Git remote or a connected source)

## 1. Create the resource

In Coolify: **Project → New Resource → Docker Compose**, pointing at this repository.

Set the Compose file path to:

```text
deploy/docker/docker-compose.ghcr.yaml
```

Coolify pulls both images; nothing is built. If you chose the build-from-source
file instead, use `deploy/docker/docker-compose.yaml` — its build context is the
repository root, so both images build in one pass.

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

Neither variable is declared in the Compose files, because the domains are not
known until Coolify has assigned them. Set them once you know the two hostnames —
before the first deploy if you set the domains yourself in step 3, otherwise
afterwards, and redeploy.

Set them to a real URL or leave them out entirely. An empty value is not the same
as an unset one: the server treats `""` as a deliberate setting and builds links
from it.

## 5. Deploy

Press **Deploy**. Coolify pulls both images and starts the two services — or builds
them first, if you chose the build-from-source file. The console waits for the
server's healthcheck.

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

Read [Upgrading](upgrading.md) first. The data volume is untouched either way.

With the published images, change `RECORD_STORE_VERSION` to the release you want
and redeploy. Nothing is rebuilt, and the version you get is the version you
named.

With the build-from-source file, push to the tracked branch and redeploy. Coolify
rebuilds the images and recreates the containers.

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| Container never becomes healthy | Missing or invalid required environment. Read the container logs — validation reports every problem at once |
| Console shows a connection error | `RECORD_STORE_API_URL` must be `http://record-store:7601`, the internal service name |
| Signed in, then immediately signed out | `RECORD_STORE_CONSOLE_SECURE_COOKIES` must be `true` behind TLS |
| Share or embed links point at `127.0.0.1` | The two base URLs are not set — see step 4 |
| Uploads fail at a size threshold | Proxy body limit |

More in [Docker and Coolify](../troubleshooting/docker-and-coolify.md).
