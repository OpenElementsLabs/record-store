# Verifying a Release

Every Record Store release is built by a GitHub Actions workflow from a signed
tag in this repository. This page is what you can check for yourself before
deploying one.

## What you can verify

| | |
| --- | --- |
| Binary archives | SHA-256 checksums covering every release asset |
| Container images | An immutable digest, an SPDX SBOM per architecture, and signed build provenance |
| The release tag | A GPG or SSH signature, verifiable with `git` |

!!! warning "Only releases built after signing was enabled"
    Provenance is produced by the build, so it exists only for images built once
    attestation was turned on. **`0.1.1` and anything earlier has none**, and
    none can be added after the fact. For those, the digest and the signed
    release tag are what you have.

    Check before relying on it: `gh attestation verify` failing on an old image
    is the expected answer, not a tampering signal.

## Provenance

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store:0.1.2 \
  --repo OpenElementsLabs/record-store
```

This answers the question a digest cannot: *was this image built by this
repository?* It checks the image against a Sigstore-signed statement naming the
workflow, the repository and the commit that produced it.

The attestation covers the multi-platform index — the digest you pull — so the
subject you verify is the subject you run.

It is also pushed to the registry as an OCI referrer, so tooling that resolves
attestations registry-side finds it without calling GitHub:

```bash
docker buildx imagetools inspect \
  ghcr.io/openelementslabs/record-store:0.1.2 --format '{{ json .Provenance }}'
```

Verify the digest you are deploying rather than a floating tag:

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store@sha256:<digest> \
  --repo OpenElementsLabs/record-store
```

## Checksums

The release publishes a `SHA256SUMS` file covering every asset attached to it.

```bash
sha256sum -c SHA256SUMS
```

On macOS:

```bash
shasum -a 256 -c SHA256SUMS
```

Run it in the directory holding the downloaded files. Files listed in
`SHA256SUMS` that you did not download report as missing; that is expected, and
`--ignore-missing` silences it.

## The release tag

Release tags are signed by the maintainer who cut them:

```bash
git fetch --tags
git tag -v v0.1.1
```

A `Good signature` line, from a key you have reason to trust, is the strongest
statement available today about who produced a release.

## Image digests

```bash
docker buildx imagetools inspect ghcr.io/openelementslabs/record-store:0.1.1
```

This prints the manifest digest and one entry per platform. Compare the digest
against the one in the release notes, then pin it — see
[Container Images](container-images.md#pinning-a-digest). A digest cannot be
repointed, so a deployment pinned to one keeps getting the same bytes even if a
tag moves.

## SBOM

Each release attaches an SPDX JSON SBOM per image and per architecture, for
example `record-store-0.1.1-linux-amd64.spdx.json`. Download it from the release
page alongside the image you are deploying.

An SBOM lists what is inside the image. It is the input to asking whether a newly
published advisory affects you, without waiting for anyone to tell you. Its
checksum is in `SHA256SUMS` like every other asset.

## Confirm what you are running

The simplest check, and the one that catches a mislabelled image:

```bash
docker run --rm --entrypoint record-store \
  ghcr.io/openelementslabs/record-store:0.1.1 --version
```

```text
record-store 0.1.1
```

The release workflow makes this same assertion against the image it just pushed,
and refuses to create the release if it fails, so a version tag cannot ship a
binary reporting a different version.
