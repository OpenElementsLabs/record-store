# Verifying a Release

Every Record Store release is built by a GitHub Actions workflow from a tagged
commit in this repository, and signed evidence of that is published with it. This
page is how you check it rather than take it on trust.

## What you can verify

| | |
| --- | --- |
| Container images | A build provenance attestation, and an SPDX SBOM per architecture |
| Binary archives | A build provenance attestation, and SHA-256 checksums |

Provenance answers *where did this come from*: which repository, which workflow,
which commit. Checksums answer *did this arrive intact*. They are different
questions and both are worth asking.

## Container provenance

Requires the [GitHub CLI](https://cli.github.com/), authenticated.

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store:0.1.1 \
  -R OpenElementsLabs/record-store
```

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store-console:0.1.1 \
  -R OpenElementsLabs/record-store
```

A successful run reports the repository, the workflow, and the commit the image
was built from. A failure means the image did not come from where it claims to,
or that the attestation is missing — treat either as a reason not to deploy it.

The attestation is bound to the image *digest*, so it survives however you refer
to the image:

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store@sha256:<digest> \
  -R OpenElementsLabs/record-store
```

Because the repository is private, `gh` must be authenticated as an account that
can read it.

## Inspect the digest and platforms

```bash
docker buildx imagetools inspect ghcr.io/openelementslabs/record-store:0.1.1
```

This prints the manifest digest and one entry per platform. Compare the digest
against the one in the release notes before deploying, and pin it — see
[Container Images](container-images.md#pinning-a-digest).

## SBOM

Each release attaches an SPDX JSON SBOM per image and per architecture, for
example `record-store-0.1.1-linux-amd64.spdx.json`. Download it from the release
page, or read the copy attached to the image itself:

```bash
gh attestation verify \
  oci://ghcr.io/openelementslabs/record-store:0.1.1 \
  -R OpenElementsLabs/record-store \
  --predicate-type https://spdx.dev/Document \
  --format json
```

An SBOM lists what is inside the image. It is the input to asking whether a newly
published advisory affects you, without waiting for anyone to tell you.

## Binary checksums

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

Binary archives also carry a provenance attestation:

```bash
gh attestation verify record-store-0.1.1-linux-amd64.tar.gz \
  -R OpenElementsLabs/record-store
```

## Confirm what you are running

The last check is the simplest, and it is the one that catches a mislabelled
image:

```bash
docker run --rm --entrypoint record-store \
  ghcr.io/openelementslabs/record-store:0.1.1 --version
```

```text
record-store 0.1.1
```

The release workflow makes this same assertion against the image it just pushed,
so a version tag cannot ship a binary reporting a different version.
