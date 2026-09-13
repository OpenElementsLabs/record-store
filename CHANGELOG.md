# Changelog

Notable changes to Record Store. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Record Store uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

The section for a released version is what the GitHub Release for that version
publishes, so keep it factual and written for the people upgrading.

## [Unreleased]

### Added

- Fuzz targets for the parsers that run before a request is authenticated, in the
  new `fuzz/` workspace: the S3 XML request bodies, the `Authorization` header, the
  presigned-URL query, the `Range` header, the ListObjectsV2 query, and bucket-name
  and object-key validation. Each target asserts an invariant rather than only the
  absence of a panic. CI builds and briefly runs all of them.

### Changed

- `tests/rust-audit.sh` now runs `cargo audit --deny warnings` with no exceptions.
  The RUSTSEC-2026-0235 exception is gone, and so is the finding it covered:
  `rust_decimal` 1.43.0 dropped the optional `rkyv` 0.7 backend that had put the
  crate in `Cargo.lock`, and `chacha20` moved off a yanked 0.10.1. `--deny warnings` makes a yanked crate a
  failure rather than a note.
- The documentation toolchain is pinned by hash. `requirements-docs.txt` now records
  an exact version and every artifact SHA-256 for each package, direct and
  transitive, and is installed with `pip install --require-hashes`.

### Documentation

- Added `SECURITY.md`: which versions receive security fixes, how to report a
  vulnerability privately through GitHub private vulnerability reporting, what a
  report should contain, and what is in and out of scope. The security and
  contributing pages now link to it instead of naming an unspecified contact.

## [0.1.1] - 2026-08-29

First release published as container images. Everything before this was built
from a repository checkout.

### Added

- Object sharing: share links, capability tokens, and unlock tickets, in the new
  `record-store-sharing` crate, with a share viewer and embed links in the console.
- Safe inline object preview for images, text, PDFs, and media, in the management
  API and the console.
- Per-bucket CORS configuration across the domain model and the S3 protocol layer.
- A documentation site built with MkDocs Material, covering getting started,
  concepts, guides, SDKs, administration, deployment, cluster operation, security,
  operations, reference, and troubleshooting, published to GitHub Pages.
- Console screens for metrics, durability, rebalance, service account detail, and
  bucket lifecycle rules; a command palette with entity commands and keyboard
  navigation; audit filtering by source IP and request ID; and a collapsible sidebar.
- A Compose file for Coolify deployments at `deploy/docker/docker-compose.yaml`.
- Container images published to the GitHub Container Registry for `linux/amd64`
  and `linux/arm64`, with SPDX SBOMs per image and architecture, and SHA-256
  checksums covering every release asset. Images are published unsigned; see
  [Verifying a Release](https://openelementslabs.github.io/record-store/deployment/verifying-releases/).

### Changed

- Renamed the product from OES to Record Store throughout: crate and binary names,
  the `RECORD_STORE_` environment variable prefix, Protobuf packages under
  `proto/record-store/`, Dockerfiles, Compose files, the example configuration file
  (now `record-store.example.toml`), documentation, and the compatibility tests.
  Deployments carrying the old environment variable prefix must be updated.
- Reworked object storage onto a streaming local filesystem backend.
- Rebuilt the console's visual language on design tokens, with accessibility and
  focus-visible improvements throughout, and a redesigned login page.
- The console now labels the deployment mode and checks cluster capability before
  offering cluster-only views.

### Fixed

- Cluster membership no longer fails outright when quorum is momentarily
  unavailable; the membership barrier waits instead.
- The console tolerates a browser that refuses `localStorage` access rather than
  failing to render the theme toggle.

### Documentation

- README documents AWS response checksum validation and path-style addressing.
- Added installation, container image, release verification, and maintainer
  release documentation for the published images.

## [0.1.0] - 2026-08-22

First tagged release, distributed as source.

[unreleased]: https://github.com/OpenElementsLabs/record-store/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/OpenElementsLabs/record-store/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/OpenElementsLabs/record-store/releases/tag/v0.1.0
