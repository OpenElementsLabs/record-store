# Container Images

Record Store publishes two production images to the GitHub Container Registry.

| Image | Contains |
| --- | --- |
| `ghcr.io/openelementslabs/record-store` | The server daemon and the `record-store` CLI |
| `ghcr.io/openelementslabs/record-store-console` | The web console |

They are separate on purpose: a headless deployment carries no frontend, and the
console upgrades on its own schedule. See [Docker](docker.md) for what each image
does at runtime.

```bash
docker pull ghcr.io/openelementslabs/record-store:0.1.1
docker pull ghcr.io/openelementslabs/record-store-console:0.1.1
```

## Architectures

Every release publishes a multi-platform manifest covering `linux/amd64` and
`linux/arm64`. One pull resolves the architecture it runs on; there are no
per-architecture tags to choose between.

```bash
docker buildx imagetools inspect ghcr.io/openelementslabs/record-store:0.1.1
```

## Tags

| Tag | Points at | Use it for |
| --- | --- | --- |
| `0.1.1` | Exactly that release, forever | Production |
| `0.1` | The newest patch of 0.1 | Automatic patch updates |
| `0` | The newest 0.x release | Rarely what you want before 1.0 |
| `latest` | The newest stable release | Trying it out |
| `sha-<short>` | One commit | Tracing an image back to source |

Pre-releases publish only their exact version and their commit tag. They never
move `latest`, and never move a floating version tag.

`latest` is a convenience, not a deployment strategy. It changes underneath a
running deployment, and it tells you nothing about what you are running.

## Version tags are immutable

A published version tag is never rebuilt. Once `0.1.1` exists, that tag keeps
pointing at that image; a fix ships as `0.1.2`.

That is a promise about the release process, not something the registry enforces.
The strong form is a digest.

## Pinning a digest

A version tag selects a *release*. A digest selects an *artifact*.

```text
ghcr.io/openelementslabs/record-store:0.1.1
    convenient, readable, and correct as long as the release process is

ghcr.io/openelementslabs/record-store@sha256:<digest>
    the exact bytes, verifiable, and impossible to repoint
```

Find the digest of what you are about to deploy:

```bash
docker buildx imagetools inspect ghcr.io/openelementslabs/record-store:0.1.1 \
  --format '{{ .Manifest.Digest }}'
```

Then pin it:

```yaml
services:
  record-store:
    image: ghcr.io/openelementslabs/record-store@sha256:<digest>
```

Every release lists both images' digests in its release notes. Use digests
wherever a deployment must be reproducible, and version tags everywhere else.

## Compose

`deploy/docker/compose.ghcr.yml` runs the server and the console from the
published images, with no build step and no repository checkout:

```bash
RECORD_STORE_VERSION=0.1.1 \
  docker compose --env-file .env -f deploy/docker/compose.ghcr.yml up -d
```

The other Compose files under `deploy/docker/` build from source, which is what
you want while developing. See [Docker Compose](docker-compose.md).

For Coolify, point the resource at the published images the same way rather than
having it build the repository — see [Coolify](coolify.md).

## Authentication

The Record Store repository is private, so its packages are too. Log in before
pulling, with a GitHub token that has `read:packages`:

```bash
echo "$GITHUB_TOKEN" | docker login ghcr.io -u <your-github-username> --password-stdin
```

!!! note "Anonymous pulls"
    Anonymous `docker pull` works only once a package's visibility has been set
    to public in its GitHub package settings, which is a deliberate one-time
    choice by the maintainers and independent of the repository's visibility.
    Until then, every pull needs a token.

## What the release publishes

Alongside the images, each release carries:

- **A build provenance attestation** per image, binding it to this repository, the
  workflow that built it, and the commit — see
  [Verifying a Release](verifying-releases.md).
- **SPDX SBOMs**, per image and per architecture, attached to the release and
  published as attestations on the image manifests.
- **Linux binary archives** with `record-store` and `record-store-server`,
  extracted from the published images so an archive and a container never hold
  different builds, and a `SHA256SUMS` file covering every asset.

macOS binaries are not published. Build from source with `cargo build --release`
— see [Installation](../getting-started/installation.md).
