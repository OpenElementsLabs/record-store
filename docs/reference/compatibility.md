# Compatibility and Support

This page says what an upgrade may and may not change, so you can decide how much
to rehearse and what to read first. It is a promise about the interfaces listed
below and nothing else.

## Version numbers

Record Store uses [semantic versioning](https://semver.org/spec/v2.0.0.html), with
these meanings from 1.0.0 on:

| Release | May change | Never changes |
| --- | --- | --- |
| Patch (1.4.**2**) | Fixes; security fixes | Any covered interface; the on-disk format |
| Minor (1.**5**.0) | Additions; deprecations; a forward-only data migration | Any covered interface in an incompatible way |
| Major (**2**.0.0) | Removal of deprecated features; incompatible changes, each listed under **Upgrading** in the changelog | — |

**Before 1.0** (0.x releases), a minor release may break any covered interface and
the changelog's **Upgrading** section lists every such break. A 0.x patch release
does not break anything.

## What is covered

| Interface | Reference |
| --- | --- |
| S3 operations, headers, and error codes listed as supported | [S3 Compatibility](s3-compatibility.md) |
| Management API under `/api/v1`: paths, request and response fields, status codes | [Management API](management-api.md) |
| Configuration file keys and their defaults | [Configuration Reference](configuration.md) |
| Environment variables | [Environment Variables](environment-variables.md) |
| CLI commands and flags, exit codes, and `--json` output | [CLI Reference](cli.md) |
| Metric names and labels | [Monitoring](../operations/monitoring.md) |
| Webhook payloads and `x-record-store-*` headers | [Events and Webhooks](../administration/events-and-webhooks.md) |
| Share and embed URL formats | [Share Links](../guides/share-links.md), [Embed Links](../guides/embed-links.md) |
| Proof bundle and audit checkpoint formats, by their version field | [Proof Bundle](proof-bundle.md), [Audit Chain](audit-chain.md) |
| Container image entrypoint, user ID, ports, and volume paths | [Container Images](../deployment/container-images.md) |
| Data written by a release, readable by every later release of the same major version | [Upgrading](../deployment/upgrading.md) |

Within a major version, **adding** is compatible: a new optional field in a JSON
response, a new optional request field, a new configuration key with a default, a
new metric, a new CLI command, a new event type. Clients should ignore fields and
event types they do not recognize.

## What is not covered

- Anything these pages do not document, including configuration keys, environment
  variables and CLI commands that are absent from the references above or hidden
  from `--help`. They can change or disappear in any release.
- Human-readable output: log messages, CLI text without `--json`, error message
  wording, metric help text. Match on codes and fields, not on prose.
- The web console's pages and layout. Its API usage follows the management API.
- The internal RPC listener and anything under `internal/` in the repository.
- Behaviour that contradicts these pages. Fixing a defect toward the documented
  behaviour is a fix, not a break, though the changelog calls it out when a client
  could have relied on the defect.

**Security exception.** A patch or minor release may refuse a request that an
earlier release accepted when accepting it was unsafe — an unsigned header, a
missing permission, a malformed digest. Each case is listed under **Security** in
the changelog with what a client should do instead.

## Data and upgrades

- **Supported upgrade path:** from the previous minor release, at any patch level,
  to the next. To skip minor releases, upgrade through each one in turn, starting
  each once. The release gates test exactly this path on every release
  (`CMP-UPGRADE`).
- **Patch releases do not change the on-disk format.** Moving between patch
  releases of the same minor release, in either direction, is a binary swap.
- **A minor release may migrate data forward** on its first start. After that, the
  previous minor release cannot open the data; rolling back means restoring the
  backup taken before the upgrade. The changelog says when a release migrates.
- **A major release** may require an upgrade from the last minor release of the
  previous major version, and says so.

## Deprecation

A covered interface is removed only in a major release, and only after it has been
deprecated in at least one earlier minor release. A deprecated feature keeps
working, logs a warning when it is used, and is listed under **Deprecated** in the
changelog with its replacement.

## Support

Fixes, including security fixes, are released for the latest minor release only.
Run the latest patch of the latest minor release; earlier releases do not receive
backports. Report vulnerabilities as [Security](../security/index.md) describes.
