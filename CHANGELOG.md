# Changelog

Notable changes to Record Store. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Record Store uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

The section for a released version is what the GitHub Release for that version
publishes, so keep it factual and written for the people upgrading.

## [Unreleased]

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
  and `linux/arm64`, with build provenance attestations, SPDX SBOMs, and SHA-256
  checksums for the binary archives.

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
