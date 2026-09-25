# Verifying a Release

Every Record Store release is built by a GitHub Actions workflow from a signed
tag in this repository. This page is what you can check for yourself before
deploying one.

## What you can verify

| | |
| --- | --- |
| Binary archives | SHA-256 checksums, and signed build provenance attached to the release |
| Container images | An immutable digest, an SPDX SBOM per architecture, and signed build provenance |
| The release tag | A GPG or SSH signature, verifiable with `git` |

!!! warning "Only releases built after signing was enabled"
    Provenance is produced by the build, so it exists only for what was built
    once attestation was turned on. **`0.1.3` and anything earlier has none.**
    For those, the digest, the checksums and the signed release tag are what you
    have.

    None can be added after the fact, and none will be. Generating provenance
    now for a build nobody observed then would be manufacturing evidence, which
    is worse than the gap it would paper over.

    So `gh attestation verify` failing on `0.1.3` or earlier is the expected
    answer, not a tampering signal. Check which release you are holding before
    reading anything into it.

## Provenance

Replace `<version>` with a release from `0.1.4` onwards:

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store:<version> \
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
  ghcr.io/openelementslabs/record-store:<version> --format '{{ json .Provenance }}'
```

Verify the digest you are deploying rather than a floating tag:

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store@sha256:<digest> \
  --repo OpenElementsLabs/record-store
```

### Binary archives

The `.tar.gz` archives carry their own provenance, so a binary downloaded and run
directly — with no registry in between — can be checked the same way:

```bash
gh attestation verify record-store-<version>-linux-amd64.tar.gz \
  --repo OpenElementsLabs/record-store
```

### Verifying without asking GitHub

The command above looks the attestation up in GitHub's attestation service. That
is convenient and it is a live dependency: it needs network, and it asks the
same organisation that hosts the download whether the download is genuine.

The release therefore also **attaches the provenance as an asset**,
`record-store-<version>-provenance.intoto.jsonl`. Download it with the archive
and verify against the file:

```bash
gh attestation verify record-store-<version>-linux-amd64.tar.gz \
  --bundle record-store-<version>-provenance.intoto.jsonl \
  --repo OpenElementsLabs/record-store
```

One bundle covers every archive in the release. Keeping it next to the archives
is what makes the evidence still checkable years later, in an air-gapped
environment, or if this repository ever disappears — none of which the service
lookup survives.

### The release refuses to publish without this

Provenance is not a step that might have run. Before the GitHub Release is
created, the workflow verifies every attestation — both image indexes, every
per-architecture SBOM, and every binary archive — against the public attestation
service, **and** checks that the bundle it is about to attach decodes and names
every archive by digest. It refuses to create the release if any of that is
missing.

The second check exists because the first one does not cover it: "the service
has provenance" and "the release ships provenance" are different claims, and
only the second survives someone archiving the download.

So if a release exists, its attestations existed at publication. If verification
fails for you now, that is worth reporting rather than working around.

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
git tag -v v0.1.3
```

A `Good signature` line, from a key you have reason to trust, is the strongest
statement available today about who produced a release.

## Image digests

```bash
docker buildx imagetools inspect ghcr.io/openelementslabs/record-store:0.1.3
```

This prints the manifest digest and one entry per platform. Compare the digest
against the one in the release notes, then pin it — see
[Container Images](container-images.md#pinning-a-digest). A digest cannot be
repointed, so a deployment pinned to one keeps getting the same bytes even if a
tag moves.

## SBOM

Each release attaches an SPDX JSON SBOM per image and per architecture, for
example `record-store-0.1.3-linux-amd64.spdx.json`. Download it from the release
page alongside the image you are deploying.

An SBOM lists what is inside the image. It is the input to asking whether a newly
published advisory affects you, without waiting for anyone to tell you. Its
checksum is in `SHA256SUMS` like every other asset.

## Confirm what you are running

The simplest check, and the one that catches a mislabelled image:

```bash
docker run --rm --entrypoint record-store \
  ghcr.io/openelementslabs/record-store:0.1.3 --version
```

```text
record-store 0.1.3
```

The release workflow makes this same assertion against the image it just pushed,
and refuses to create the release if it fails, so a version tag cannot ship a
binary reporting a different version.
