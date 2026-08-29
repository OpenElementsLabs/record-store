# Verifying a Release

Every Record Store release is built by a GitHub Actions workflow from a signed
tag in this repository. This page is what you can check for yourself before
deploying one.

## What you can and cannot verify

| | |
| --- | --- |
| Binary archives | SHA-256 checksums covering every release asset |
| Container images | An immutable digest, and an SPDX SBOM per architecture |
| The release tag | A GPG or SSH signature, verifiable with `git` |
| Container images | **No cryptographic signature or provenance attestation** |

!!! warning "Images are published unsigned"
    There is no `gh attestation verify` to run against a Record Store image, and
    no `cosign` signature. GitHub's artifact attestation service is not available
    to this repository under its current plan and visibility, and publishing an
    unsigned provenance blob would suggest a guarantee that does not exist —
    anyone who can write to the registry could produce the same thing.

    What that means in practice: a digest proves an image has not *changed*, but
    nothing here proves it was *built by this repository*. Treat the registry
    itself, and the accounts that can push to it, as part of your trust boundary.

    If this matters for your deployment, say so on the issue tracker. Making the
    repository public, or moving the organisation to a plan that includes
    attestations, would enable signed provenance without any change to the
    release pipeline's shape.

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
