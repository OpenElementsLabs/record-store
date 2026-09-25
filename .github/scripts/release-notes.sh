#!/usr/bin/env bash
# Composes the GitHub Release body for a Record Store version.
#
# The human part comes from CHANGELOG.md so the release notes and the changelog
# cannot drift apart, and an empty section fails the release rather than
# publishing a version nobody described. Everything below it is generated from
# what the workflow actually pushed, including the digests, so the notes cannot
# name an artifact that does not exist.
#
# Required environment:
#   VERSION          release version, without a leading v
#   REPOSITORY       owner/name, used in the verification commands
#   SERVER_IMAGE     server image, without a tag
#   SERVER_DIGEST    published server index digest
#   CONSOLE_IMAGE    console image, without a tag
#   CONSOLE_DIGEST   published console index digest
# Optional:
#   ASSET_DIRECTORY  directory of release assets; a checksums section is written
#                    only when it holds a SHA256SUMS file
set -euo pipefail

: "${VERSION:?}" "${REPOSITORY:?}"
: "${SERVER_IMAGE:?}" "${SERVER_DIGEST:?}" "${CONSOLE_IMAGE:?}" "${CONSOLE_DIGEST:?}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

changelog="$(
  awk -v version="$VERSION" '
    $0 ~ "^## \\[" version "\\]" { capture = 1; next }
    capture && /^## /            { exit }
    capture                      { print }
  ' "$root/CHANGELOG.md" | sed -e '/./,$!d' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}'
)"

if [[ -z "$changelog" ]]; then
  echo "CHANGELOG.md has no section for $VERSION" >&2
  exit 1
fi

cat <<NOTES
$changelog

## Containers

Published to the GitHub Container Registry for \`linux/amd64\` and \`linux/arm64\`.
One pull resolves the architecture it is run on.

\`\`\`bash
docker pull $SERVER_IMAGE:$VERSION
docker pull $CONSOLE_IMAGE:$VERSION
\`\`\`

A version tag selects a release; a digest selects an exact artifact. Pin the
digest in production:

\`\`\`text
$SERVER_IMAGE@$SERVER_DIGEST
$CONSOLE_IMAGE@$CONSOLE_DIGEST
\`\`\`

\`$VERSION\` is immutable. A fix is released as the next patch version, never as a
rebuild of this one.

## Verification

Every image and every binary archive carries signed build provenance, naming the
workflow, the repository and the commit that produced it. The release refuses to
publish if any of it is missing.

\`\`\`bash
gh attestation verify oci://$SERVER_IMAGE@$SERVER_DIGEST --repo $REPOSITORY
gh attestation verify record-store-$VERSION-linux-amd64.tar.gz --repo $REPOSITORY
\`\`\`

The same provenance is attached here as \`record-store-$VERSION-provenance.intoto.jsonl\`,
so it can be kept alongside the bytes and checked without asking GitHub about a
file GitHub is also hosting:

\`\`\`bash
gh attestation verify record-store-$VERSION-linux-amd64.tar.gz \\
  --bundle record-store-$VERSION-provenance.intoto.jsonl \\
  --repo $REPOSITORY
\`\`\`

Confirm the image reports the version it is tagged with:

\`\`\`bash
docker run --rm --entrypoint record-store $SERVER_IMAGE:$VERSION --version
\`\`\`

Inspect the published manifest, its digest, and its platforms:

\`\`\`bash
docker buildx imagetools inspect $SERVER_IMAGE:$VERSION
\`\`\`
NOTES

if [[ -n "${ASSET_DIRECTORY:-}" && -f "${ASSET_DIRECTORY}/SHA256SUMS" ]]; then
  cat <<'NOTES'

Verify the downloadable archives against the published checksums:

```bash
sha256sum -c SHA256SUMS      # macOS: shasum -a 256 -c SHA256SUMS
```
NOTES
fi

cat <<NOTES

## Assets

| Asset | What it is |
| --- | --- |
| \`record-store-$VERSION-linux-ARCH-musl.tar.gz\` | Statically linked binaries. No libc dependency, so they run on any distribution. |
| \`record-store-$VERSION-linux-ARCH.tar.gz\` | glibc binaries, taken from the published image so the archive and the container hold the same build. |
| \`record-store_${VERSION}_ARCH.deb\` | Debian and Ubuntu package: systemd unit, configuration, per-machine generated credentials. |
| \`record-store-$VERSION-1.ARCH.rpm\` | The same for RHEL, Rocky, Fedora and openSUSE. |
| \`record-store-$VERSION.tgz\` | The Helm chart, for installs that cannot reach a registry. |
| \`*.spdx.json\` | An SPDX SBOM per image and per architecture. |
| \`record-store-$VERSION-provenance.intoto.jsonl\` | SLSA provenance for the glibc archives. |
| \`record-store-$VERSION-release-gates.json\` / \`.md\` | The release gate decision that allowed this release. |
| \`SHA256SUMS\` | Checksums for every asset here. |

\`ARCH\` is \`amd64\` or \`arm64\` (\`x86_64\`/\`aarch64\` for the rpm). Both archives
contain \`record-store\` and \`record-store-server\`.

The packages carry statically linked binaries and declare no libc dependency, so
they install on Debian 11 and RHEL 8 onwards.

The provenance covers the glibc archives, which are extracted from the attested
image. The static archives and the packages are compiled separately and carry no
provenance yet; verify them against \`SHA256SUMS\`.

The release tag is signed too. See
[Verifying a Release](https://openelementslabs.github.io/record-store/deployment/verifying-releases/).

macOS builds are not published; build from source with \`cargo build --release\`.

## Helm chart

\`\`\`bash
helm install record-store \\
  oci://ghcr.io/openelementslabs/charts/record-store --version $VERSION
\`\`\`
NOTES
