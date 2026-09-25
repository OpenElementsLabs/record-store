# Changelog

Notable changes to Record Store. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Record Store uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

The section for a released version is what the GitHub Release for that version
publishes, so keep it factual and written for the people upgrading.

## [Unreleased]

### Added

- **Release gates decide releases.** `release/gates.toml` defines every check a
  release must pass: the guarantee it protects, its workload and failure
  injection, what it measures against which limit and why, where its evidence is
  kept, and whether it blocks. The gates run on every change (`pr` and
  `integration` stages), nightly and weekly (`scheduled`), and for a release
  candidate, and one evaluator turns their results into a decision. A gate that
  failed, was skipped, never ran, ran an older definition of itself, ran a lighter
  workload, or ran binaries other than the candidate's blocks. Exceptions are
  reviewed, expiring files; nothing waives a gate automatically.

  New real-binary gates start the built server, damage or interrupt it the way a
  crash, a bad disk or an operator mistake would, and check final bytes and state
  rather than status codes: crash recovery under SIGKILL, backup and restore with
  every refusal the recovery guide promises, upgrade from the real 0.1.3 binaries
  and the documented rollback, integrity on read, refusal of unsupported S3
  operations, overload and slow clients, and secret redaction across logs, APIs,
  CLI output, the data directory and backups.

  A release now runs the gates on the tag commit before publishing, runs the
  candidate gates again against the binaries extracted from the published image,
  smoke-tests both `linux/amd64` and `linux/arm64` (non-root, clean SIGTERM exit,
  persistence across a restart), and attaches the decision to the release as
  `record-store-<version>-release-gates.json` and `.md`. See
  [Release Gates](docs/contributing/release-gates.md).


- **Releases ship their provenance as an asset, not only as an API record.**
  The build already produced SLSA provenance for every binary archive, but it
  existed only in GitHub's attestation service. A downloader could verify with
  `gh attestation verify` while online, and had nothing to keep: no file to
  archive next to the bytes, nothing checkable in an air-gapped environment or
  after this repository is gone, and nothing on the release page for a supply
  chain scanner to find.

  The same bundle is now attached to the release as
  `record-store-<version>-provenance.intoto.jsonl`, covering every archive, and
  verified against a downloaded copy with
  `gh attestation verify <archive> --bundle <bundle> --repo <repo>`.

  The release refuses to publish unless that bundle decodes and names every
  archive by digest — a separate check from the service lookup, because "the
  service has provenance" and "the release ships provenance" are different
  claims and only the second one survives being archived.

  **`0.1.3` and earlier have no provenance and never will.** Attestation was
  turned on after `0.1.3` was cut, and generating provenance now for a build
  nobody observed then would be manufacturing evidence.

- **The audit Merkle tree binds its own size, and the root has its own domain.**
  Two changes to the checkpoint and tree layer, made before anything writes a
  checkpoint to disk, because changing a hash preimage after signed checkpoints
  exist costs a format version and a migration.

  A checkpoint now carries `leaf_count`, and it is inside the checkpoint hash
  preimage — fixed-width big-endian, in a defined position — and inside the signed
  bytes of a proof bundle. Path verification takes the leaf count as a required
  argument and **rejects** a path whose length is not the one that leaf index in a
  tree of that size must produce. Odd nodes are promoted rather than duplicated,
  which is the right choice, but it makes path length vary by leaf position: a
  verifier that does not know the leaf count cannot tell a legitimate path from one
  built against a tree of a different size. A checkpoint whose `leaf_count`
  disagrees with the range it claims is reported as a failure — that disagreement is
  what a record dropped from the tree looks like from outside, and every proof for
  the records that remain still folds to the published root.

  The root is now hashed under a third domain prefix (`0x02`), distinct from the
  leaf (`0x00`) and node (`0x01`) prefixes and applied once at the top, including
  for a single-leaf tree. Without it a one-record tree roots at its own leaf, so any
  record hash could be presented as a root that an empty inclusion path verifies
  against. **A tree over zero leaves has no root and a checkpoint covering no
  records is never written**; one is refused at construction rather than left to
  whatever falls out.

  Known-answer vectors for trees of 1, 2, 3, 5, 8 and 9 leaves are committed as data
  files under `crates/record-store-audit/tests/vectors/merkle/`, computed by a
  separate implementation of the written rules rather than printed from the encoder,
  so an independent verifier can reproduce them from the specification alone.

  Documentation: [Audit Chain and Checkpoints](docs/reference/audit-chain.md), which
  specifies the three prefixes and where each applies, the promotion rule, the root
  rule including the single-leaf and zero-leaf cases, the checkpoint preimage field
  order with widths and endianness, and the inclusion-path encoding including the
  direction bit per step.

  The proof bundle format gains `checkpoint.leaf_count` in the JSON and in the
  canonical encoding signatures cover. No format version bump: the history and
  checkpoint sections have never been emitted as anything but `unavailable`, so no
  bundle in existence carries a checkpoint.

- **Auditor-facing audit export.** `record-store audit-export export --from <ts>
  --to <ts> --format json|csv --out <dir>` writes a directory holding the records, a
  manifest, the covering checkpoint roots, and a `SHA256SUMS` over all three. Records
  are paged from the store and streamed to disk on both sides, so the range is never
  held in memory.

  The range is `[from, to)`. Adjacent exports therefore tile: January and February
  together contain every record exactly once, with nothing duplicated at the boundary
  and nothing lost. The underlying audit query treats its upper bound as inclusive,
  so the export filters the boundary itself rather than relying on timestamp
  precision.

  `--format json` is a single streamed JSON array that parses with any JSON reader;
  `--format csv` is RFC 4180 with a pinned column order and metadata in one JSON
  column, so the column set never depends on the data.

  Requesting an export writes an audit record naming who asked, the range, the format,
  and an export id. It is written when the export is **authorized**, not when the
  bytes finish, and an export that cannot be recorded is refused rather than performed
  untracked. An export whose range includes the present will contain the record of
  itself.

- **Retention report.** `record-store audit-export retention-report` reports which
  buckets have Object Lock, which versions are currently held, and when each retention
  expires. It distinguishes `held` from `elapsed` — a lock record outlives the
  retention it describes, and reporting an expired retention as active would overstate
  what is protected. The scan walks the Object Lock table, which holds only locked
  versions, so a deployment with a million objects and ten locks pays for ten. Bounded,
  with truncation reported rather than silent.

- Both are readable with the existing **auditor** management role and available over
  the management API (`GET /api/v1/audit/export`, `/api/v1/audit/export/manifest`,
  `/api/v1/reports/retention`), not only the CLI.

- Documentation: [Audit Export](docs/administration/audit-export.md), including what an
  export does and does not prove. `SHA256SUMS` establishes that the copy reached you
  unaltered; it does not establish that the log was not edited before the copy was
  taken. **`checkpoints.json` is always written and currently reports
  `chain_not_enabled`**, because the tamper-evident audit chain is not built yet — an
  explicit status rather than an omitted file, so an unanchored copy is not mistaken
  for an anchored one.

- `GET /api/v1/system/metrics/history` returns the last hour of counter readings, taken
  by the server every 15 seconds and held in a bounded in-memory ring (240 samples, a
  few tens of kilobytes). Samples are counters rather than rates, the way a scraper
  sees them.

- **Portable proof bundles.** `record-store verify object <bucket> <key>
  [--version-id ID] --proof <out.json>` emits a signed JSON document describing one
  immutable object version: its identity, the SHA-256 recorded at write time, the
  deployment's public verification key, and a bundle format version. It carries no
  capability tokens, no credentials, and not the payload.

  `record-store verify proof <out.json> --object ./file` checks one **offline** —
  no server, no network, no credential. It streams the file past SHA-256 rather
  than loading it, verifies the Ed25519 signature, and prints every check it
  performed alongside every one it could not, so a passing result never implies
  more than it established. It exits non-zero on failure.

  The signing key is derived from `RECORD_STORE_CREDENTIAL_MASTER_KEY` under the
  domain separation string `record-store/proof-signing/v1`, so it is distinct from
  the credential, capability, webhook, and object-encryption keys and survives a
  restore from backup. Without a master key the deployment reports bundles as
  unavailable rather than emitting an unsigned one, which would be mistaken for a
  signed one.

  The format is specified in [`docs/reference/proof-bundle.md`](docs/reference/proof-bundle.md),
  including the canonical byte encoding signatures cover, the Merkle rules, the key
  derivation, and a worked example, so an independent implementation is possible.

  **The audit history, checkpoint, and anchor sections are defined in the format but
  are reported as `unavailable` in this release**, because the tamper-evident audit
  chain they draw on is not built yet. They are an explicit `status` rather than an
  omitted field: a missing section reads as "nothing happened", where this reads as
  "this was not checked". Bundles issued now stay parseable by later verifiers.

  A bundle is not a certificate of authenticity. The signing key derives from the
  deployment's own master key, so it is evidence against alteration in transit and
  against a third-party forgery, not against the deployment's own operator. Without
  the deployment's public key obtained out of band, the verifier reports the
  deployment's identity as **not established** rather than passing that check.

- **Object Lock, with AWS semantics.** `GOVERNANCE` and `COMPLIANCE` retention, legal
  holds, per-bucket default retention, and a governance bypass that needs its own policy
  permission. The S3 surface adds `CreateBucket` with
  `x-amz-bucket-object-lock-enabled: true`, `Put`/`GetObjectLockConfiguration`,
  `Put`/`GetObjectRetention`, `Put`/`GetObjectLegalHold`, the
  `x-amz-object-lock-{mode,retain-until-date,legal-hold}` request headers on `PutObject`
  and `CreateMultipartUpload`, and the matching response headers on `GetObject` and
  `HeadObject`.

  Enforcement runs inside the metadata transaction that would remove the version, so a
  retention placed concurrently cannot be raced. A `COMPLIANCE` retention may only be
  extended, never shortened or removed, by anyone including the root credential. A
  `GOVERNANCE` retention yields only to `x-amz-bypass-governance-retention: true`
  presented by a credential holding the new `s3:BypassGovernanceRetention` permission,
  and every bypass — successful or refused — writes an audit record. A legal hold blocks
  deletion independently of retention in both modes, and has no bypass at all.

  Deleting a *version* under retention returns `403 AccessDenied`; placing a delete
  marker over it stays allowed, because the marker destroys nothing. Overwriting a key
  publishes a new version and never mutates a locked one.

- New policy actions: `s3:GetObjectRetention`, `s3:PutObjectRetention`,
  `s3:GetObjectLegalHold`, `s3:PutObjectLegalHold`, and `s3:BypassGovernanceRetention`.
  Bucket-level Object Lock configuration sits under the existing `s3:ManageBucket`,
  alongside versioning and CORS. Existing stored policies are unaffected.

- An `[object_lock]` configuration section, with
  `RECORD_STORE_OBJECT_LOCK_CLOCK_WATERMARK_INTERVAL_SECONDS` (default 60) and
  `RECORD_STORE_OBJECT_LOCK_CLOCK_BACKWARDS_TOLERANCE_SECONDS` (default 5). Record Store
  persists a monotonic high-water mark of observed wall-clock time and refuses
  retention-*releasing* operations with `503 ServiceUnavailable` while the clock is
  behind it, logging a warning. Reads, ordinary writes, and operations that only add
  protection keep working. The mark is not substituted for the wall clock, because a
  single bogus forward jump would then become a permanent licence to delete early.

  The worker that refreshes the mark races each observation against the shutdown signal
  rather than awaiting it. In a cluster that observation is a consensus proposal, and a
  node mid-join has no leader to accept one, so awaiting it would make graceful shutdown
  wait for a write that might never land. It also skips the immediate first tick a tokio
  interval delivers, which kept a replicated write out of the boot window.

- `record-store bucket object-lock show|set-default|status`, and the management API
  routes behind them. Per-object lock state is read-only on the management plane by
  design: placing or releasing a retention is an S3 action governed by S3 policy, and a
  second door on port 7601 would make `s3:BypassGovernanceRetention` meaningless. The
  auditor role may read lock state; the storage-administrator role may set the bucket
  default.

- Documentation: [Object Lock](docs/administration/object-lock.md) and
  [Object Lock and Trust](docs/security/object-lock.md). The second says plainly what a
  retention date does and does not prove — it is enforced by Record Store, not by the
  filesystem, and it is not evidence against an operator with access to the data
  directory.

- Debian and RPM packages for `amd64` and `arm64`, published with every release.
  They install the server and CLI, a hardened systemd unit, a configuration file
  that upgrades never overwrite, and generate credentials unique to the machine
  on first install. Nothing starts until an operator enables it. The binaries
  inside are statically linked, so the packages declare no libc dependency and
  install on Debian 11 and RHEL 8 onwards.

- Statically linked Linux binary archives (`…-musl.tar.gz`) beside the existing
  glibc ones. The glibc archives come from the container image and need a
  distribution at least as new as Debian 12; these run anywhere.

- A Helm chart, published to `ghcr.io/openelementslabs/charts/record-store` and
  attached to each release for air-gapped installs. It runs the server as a
  StatefulSet with per-node volumes and stable peer addresses, the console as a
  Deployment, and keeps the management API `ClusterIP`-only — a rule CI asserts.
  Standalone and multi-node clusters are both supported: a joining node obtains
  a short-lived join token from node 0 through an init container, because a join
  token is issued by the running cluster and cannot be created in advance.

### Changed

- **The console's metrics charts draw immediately instead of filling in over minutes.**
  Record Store exposes counters, so a rate can only come from comparing two readings.
  The console did all of that comparing itself, which meant it could only show a rate
  it had personally watched happen: nothing on the first paint, one point after the
  second poll, a trend that took minutes to fill, and a page reload that threw the
  whole window away. It now seeds its window from the server's readings, so the waiting
  happens in the background before anyone opens the page.

  The data path was never the problem — that endpoint answers in about five
  milliseconds and does not get slower as the store grows.

  Seeding is a convenience and is treated as one: a server too old to know the path, or
  any unrecognisable response, leaves the screen working exactly as it did before
  rather than failing. The history is in memory only and resets on restart, which the
  endpoint reports through `started_at` rather than hiding.

- The metadata schema moves from version 4 to 5, adding an Object Lock table and a clock
  table. The migration is ordered and non-destructive: nothing is rewritten, a v4 bucket
  decodes with no Object Lock configuration, and a v4 version decodes as unlocked. An
  existing v4 deployment starts, migrates, and keeps serving every object unchanged.

- The lifecycle worker skips any version under retention or a legal hold, writes an audit
  record naming the rule and the reason, and continues the scan rather than aborting.
  `LifecycleRunResult` gains a `skipped` count, kept separate from `failures` because
  nothing went wrong. A lifecycle scan never carries a governance bypass.

- Bucket versioning can no longer be suspended while Object Lock is enabled
  (`409 InvalidBucketState`). Suspending it would make the next write replace the null
  version in place, which is the history a retained version is meant to be safe from.

- Object Lock cannot be enabled on an existing bucket. This is deliberate and stricter
  than current AWS: enabling it later would claim protection over versions that were
  written without it.

- `x-amz-object-lock-mode`, `x-amz-object-lock-retain-until-date`, and
  `x-amz-object-lock-legal-hold` are now honoured rather than rejected. Any *other*
  `x-amz-object-lock-*` header is still an unimplemented semantic and still returns
  `NotImplemented`.

- Lock errors reaching the S3 surface through the delete path now return their intended
  status. They previously would have surfaced as `500 InternalError`, telling a client to
  retry something meant never to succeed.

- **Every body digest a client sends is verified.** `Content-MD5` and
  `x-amz-checksum-crc32`, `-crc32c` and `-sha1` were accepted and ignored — as
  in 0.1.3 — so a client that sent one believed a comparison had happened when
  none had. They are now verified on uploads, multipart parts and every XML
  request body, and a mismatch is refused with `400 BadDigest` without storing
  anything; `x-amz-checksum-sha256` is also checked on XML bodies. A verified
  `x-amz-checksum-*` is echoed in the response. **A request carrying
  `x-amz-checksum-crc64nvme` is now refused with `NotImplemented`** instead of
  being stored unverified; configure the client to use CRC32, CRC32C, SHA-1 or
  SHA-256. Two different `x-amz-checksum-*` algorithms on one request are
  refused, as S3 refuses them.

- The release workflow builds, installs and runs the Linux packages before it
  publishes anything, and CI lints the Helm chart, validates every render shape
  against the Kubernetes schemas, and installs it on a throwaway cluster.

### Fixed

- **The upgrade guide can now be followed from 0.1.3.** It told operators to run
  `record-store server backup` before stopping the server, which a backup refuses;
  used commands that 0.1.3 does not ship; and passed `record-store server
  check-config` to an image whose entrypoint is already `record-store server`, so
  the configuration check always failed. The guide now stops first, backs up and
  verifies with the new image, and explains that 0.1.3 refuses an upgraded data
  directory by exiting with a panic that leaves the directory unchanged.

- **A crash can no longer leave the server unable to start.** A process killed
  between creating and writing a publication record under `tmp/` left an empty
  record, and every later start refused it with `storage publication record
  encoding failed` — as 0.1.3 does. Records are now written under a temporary
  name and renamed into place, and one that cannot be read is recovered by the
  object id in its file name: its payload is only ever moved into place after
  the record is complete, so recovery is exact. If a 0.1.3 deployment is stuck
  this way, upgrading clears it.
- **Paging through storage events no longer skips events.** Each page's cursor
  named the first event it did *not* return, so the next page began after it
  and one event was lost at every page boundary — also in 0.1.3. Webhook
  delivery was never affected.

### Security

- **Every `x-amz-*` header must now be signed.** A request could carry `x-amz-*`
  headers its signature did not cover, and the server acted on them. That mattered
  most for presigned URLs, whose holder could add terms the signer never approved.
  Such a request is now refused with `403 AccessDenied` (`There were headers present
  in the request which were not signed`), as AWS does. For header authentication this
  includes `x-amz-date` and `x-amz-content-sha256`. `Content-MD5` may still be left
  unsigned.
- **Setting Object Lock at write time needs the Object Lock permissions.**
  `x-amz-object-lock-mode` and `x-amz-object-lock-retain-until-date` on a PUT, copy,
  or multipart initiation now require `s3:PutObjectRetention`, and
  `x-amz-object-lock-legal-hold` requires `s3:PutObjectLegalHold`. Before, `s3:PutObject`
  alone was enough to write a `COMPLIANCE` version nobody could delete. A bucket's
  default retention still needs only `s3:PutObject`.
- **Copying a named version needs `s3:GetObjectVersion`.** A copy source with
  `?versionId=` was checked against `s3:GetObject`, so an account that could read only
  current objects could read any version by copying it.

**Potentially breaking.** A client that sent unsigned `x-amz-*` headers is now
refused. The AWS SDKs sign every such header, so this affects hand-built requests and
presigned URLs whose holder adds headers: sign the URL with them. An account that
set locks at write time with only `s3:PutObject`, or copied named versions with only
`s3:GetObject`, needs the permissions above. See
[Policies](docs/administration/policies.md).

## [0.1.3] - 2026-09-16

A patch release that prepares every database for the next one. It changes no
configuration and no API, and it is worth installing promptly: the release that
follows cannot read a database this one has not opened.

### Changed

- redb moves from 2.6.3 to 4.x, which reads only file format v3. **Upgrade to
  0.1.3 and start it once before upgrading to this release**: 0.1.3 converts
  each database to v3, and the conversion is unavailable here because redb 4
  removed `Database::upgrade` along with the ability to read v2. A database that
  is still v2 is refused at startup with a message naming 0.1.3 rather than a
  file format number.
- **This release cannot be rolled back to 0.1.2 or earlier.** Those releases
  cannot read the v3 file that 0.1.3 produced. 0.1.3 itself remains a safe
  rollback target.

- Every redb database is migrated from file format v2 to v3 when it is opened.
  redb 3.0 dropped the ability to read v2, and every release up to 0.1.2 wrote
  it, so a later redb 4 upgrade would otherwise meet a file it cannot open. The
  migration runs once per database on first start, is a no-op afterwards, and
  the redb version shipped here still reads a migrated file — this release
  remains one you can go back to. **Upgrade to this release before any release
  that carries redb 4.**

## [0.1.2] - 2026-09-16

A patch release. The S3 layer accepts the AWS SDK for Java v2's defaults, and a
rustls advisory is closed. No configuration or data changes are required.

### Added

- AWS SDK for Java v2 compatibility tests (Java 21, SDK 2.54.12), run by
  `tests/compatibility/run.sh` and in CI alongside the boto3, JavaScript, and Go
  suites, and a Java client setup page. Java was the only major SDK without
  coverage, and the only one whose defaults the suite could not otherwise exercise.

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

### Fixed

- A PUT authenticated with a SigV4 `Authorization` header was refused when it
  carried `x-amz-content-sha256: UNSIGNED-PAYLOAD`, which is a valid SigV4 value
  and the AWS SDK for Java v2 default. Such requests are now accepted; the
  signature, credentials, and any supplied checksum are still verified, and only
  the body is left uncovered by the signature.
- Unsupported AWS streaming payloads — `aws-chunked` framing and trailing
  checksums — returned a generic `400 InvalidRequest`. They now return
  `501 NotImplemented`, as the documentation promises for unsupported operations,
  with a message naming the encoding and the setting to change. A malformed
  `x-amz-content-sha256` likewise names the header it rejected instead of
  reporting `Invalid Request`.

- A `Range` header naming a last byte past the end of the object returned a range
  reaching past EOF from `parse_range`. Responses were unaffected — the range was
  truncated again before the body or the `Content-Range` header were built — but the
  value was valid only because of that later call. It is now clamped where it is
  parsed, as the `bytes=-N` suffix form already was. Found by the `s3_range_header`
  fuzz target.

### Security

- `rustls` moves from 0.23.43 to 0.23.45, closing RUSTSEC-2026-0285: releases
  before 0.23.45 accept TLS 1.3 handshake messages across encryption level
  boundaries. It reaches Record Store transitively through `reqwest`, so only the
  lockfiles changed.

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

[unreleased]: https://github.com/OpenElementsLabs/record-store/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/OpenElementsLabs/record-store/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/OpenElementsLabs/record-store/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/OpenElementsLabs/record-store/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/OpenElementsLabs/record-store/releases/tag/v0.1.0
