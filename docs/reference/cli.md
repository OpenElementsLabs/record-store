# CLI Reference

Two binaries ship with Record Store:

| Binary | Purpose |
| --- | --- |
| `record-store` | The operational CLI, including `record-store server` |
| `record-store-server` | The daemon alone; accepts only `--config` |

Use `record-store` for everything. `record-store server` starts the same daemon.

## Global options

| Option | Effect |
| --- | --- |
| `--json` | Emit JSON suitable for automation |

`--json` is global and may appear anywhere.

## Endpoints

Every command that talks to a running server takes `--endpoint`, defaulting to
`http://127.0.0.1:7601`.

!!! note "`--endpoint` goes after the subcommand"
    ```bash
    record-store bucket list --endpoint https://management.example.com
    ```

    Not `record-store --endpoint ... bucket list`.

## Authentication

The CLI reads credentials from the environment:

| Variable | Used for |
| --- | --- |
| `RECORD_STORE_MANAGEMENT_TOKEN` | Bearer authentication — preferred |
| `RECORD_STORE_ROOT_ACCESS_KEY` + `RECORD_STORE_ROOT_SECRET_KEY` | Basic authentication fallback |

```bash
export RECORD_STORE_MANAGEMENT_TOKEN=<your-management-token>
record-store bucket list --endpoint https://management.example.com
```

## `version`

```bash
record-store version
```

```text
record-store 0.2.0
commit 3f1c9a…
```

Names the release and the commit the binary was built from (`unknown` for a build
that was given neither a checkout nor `RECORD_STORE_BUILD_COMMIT`). `--json version`
prints both as an object. `record-store --version` prints the first line only, for
scripts that compare it with a release.

## `server`

Starts the server, or operates on its data offline.

```bash
record-store server --config /etc/record-store/config.toml
```

`--config` may also come from `RECORD_STORE_CONFIG_FILE`. With no config file, the
server runs on defaults plus environment variables.

### `server check-config`

```bash
record-store server --config /etc/record-store/config.toml check-config
```

Loads the file, applies the environment, validates, and exits. Binds nothing and writes
nothing.

### `server doctor`

```bash
record-store server --config /etc/record-store/config.toml doctor
```

Reports whether this machine can run the configured deployment: the data directory and
its permissions, whether the temporary directory allows atomic publication, the on-disk
storage format, free space, the configured addresses, and which key material is
present. Starts nothing, opens no database, prints no secret value.

Exits 0 when nothing failed and 7 when something did. `--json` emits the report.

### `server backup`

```bash
record-store server backup /backups/2026-09-22 [--replace-incomplete]
```

Copies payloads, metadata, and system records into one destination with a manifest.
Takes the exclusive data lock, so **the server must be stopped**. The destination must
be empty or absent; a destination holding a completed backup is never overwritten.

### `server verify-backup`

```bash
record-store server verify-backup /backups/2026-09-22 --level full
```

Checks a backup without restoring it, at level `manifest`, `checksums` (the default),
or `full`. Exits 3 when the backup is not usable.

### `server restore`

```bash
record-store server restore /backups/2026-09-22 --level full
```

Verifies the backup, then stages and restores every component into an empty data
directory. Refuses to write into a data directory that already holds one.

### `server backup-metadata`, `server restore-metadata`

Deprecated. They copy metadata only, and warn on use. See
[Backup and Restore](../operations/backup-and-restore.md).

## `status`

```bash
record-store status --endpoint https://management.example.com
```

Checks `/ready`, then prints system information when a management token is present.
Exits non-zero if the server is not ready, which is what makes it usable as a container
healthcheck with no credential at all.

## `bucket`

```bash
record-store bucket list --endpoint <endpoint>
record-store bucket create <name> --endpoint <endpoint>
record-store bucket delete <name> --endpoint <endpoint>

record-store bucket versioning get <name> --endpoint <endpoint>
record-store bucket versioning enable <name> --endpoint <endpoint>
record-store bucket versioning suspend <name> --endpoint <endpoint>

record-store bucket object-lock show <name> --endpoint <endpoint>
record-store bucket object-lock set-default <name> --mode GOVERNANCE --days 365 --endpoint <endpoint>
record-store bucket object-lock set-default <name> --mode COMPLIANCE --years 7 --endpoint <endpoint>
record-store bucket object-lock status <name> <key> [--version-id <id>] --endpoint <endpoint>
```

## `audit-export`

```bash
record-store audit-export export --from <rfc3339> --to <rfc3339> \
  --format json|csv --out <dir> --endpoint <endpoint>
record-store audit-export retention-report --endpoint <endpoint>
record-store audit-export verify-chain [--from <sequence>] [--limit <n>] --endpoint <endpoint>
```

`export` writes a directory holding the records, a manifest, the covering
checkpoint roots, and a `SHA256SUMS` over all three. The range is `[from, to)`,
so adjacent exports tile without duplicating a boundary record.

`verify-chain` recomputes the audit hash chain and reports whether the log still
verifies. A long log is walked in spans: follow `next_from` until it is absent. What
it does and does not establish is set out in
[Audit Log](../administration/audit-log.md#checking-the-log-has-not-been-edited).

All three are readable with the auditor token. See
[Audit Export](../administration/audit-export.md).

`delete` requires the bucket to be empty. There is no `versioning disable` — see
[Versioning](../concepts/versioning.md).

Object Lock is enabled when a bucket is created, over S3, and never afterwards. These
commands read it and set the bucket default; `set-default` takes exactly one of `--days`
or `--years`.

`object-lock status` is read-only, and there is deliberately no command to place or
release a retention. That is an S3 action governed by S3 policy, and a management-plane
equivalent would let a caller refused over S3 succeed simply by changing port. See
[Object Lock](../administration/object-lock.md).

## `service-account`

```bash
record-store service-account list --endpoint <endpoint>
record-store service-account create <name> --endpoint <endpoint>
record-store service-account inspect <id> --endpoint <endpoint>
record-store service-account enable <id> --endpoint <endpoint>
record-store service-account disable <id> --endpoint <endpoint>
record-store service-account revoke <id> --endpoint <endpoint>
```

`create` always prints JSON, because it contains the secret key — shown once.

`revoke` **permanently deletes** the account and its access-key lookups. Use `disable`
if you might want it back.

## `credential`

```bash
record-store credential rotate <account-id> --endpoint <endpoint>
record-store credential enable <account-id> <credential-id> --endpoint <endpoint>
record-store credential disable <account-id> <credential-id> --endpoint <endpoint>
record-store credential temporary <account-id> \
  --expires-in-seconds 3600 --endpoint <endpoint>
```

`rotate` issues a **new** credential alongside the existing one. The old one keeps
working until you disable it.

`--expires-in-seconds` defaults to 3600 and must be between 60 and 86400.

## `policy`

```bash
record-store policy list --endpoint <endpoint>
record-store policy create <file> --endpoint <endpoint>
record-store policy attach <policy-id> <account-id> --endpoint <endpoint>
record-store policy detach <policy-id> <account-id> --endpoint <endpoint>
```

`create` takes a path to a JSON policy document. See
[Policies](../administration/policies.md).

## `webhook`

```bash
record-store webhook list --endpoint <endpoint>
record-store webhook create <file> --endpoint <endpoint>
record-store webhook deliveries --limit 100 --endpoint <endpoint>
```

`create` takes a path to a JSON document and returns the signing secret **once**.

## `audit`

```bash
record-store audit \
  --limit 100 \
  --principal <principal> \
  --operation <operation> \
  --endpoint <endpoint>
```

`--limit` defaults to 100; the API accepts 1–1000. The API supports more filters than
the CLI exposes — see [Audit Log](../administration/audit-log.md).

## `verify`

```bash
record-store verify object <bucket> <key> --endpoint <endpoint>
record-store verify bucket <bucket> --endpoint <endpoint>
```

Reads the bytes back and compares them to the stored checksum. `verify bucket` reads
every object — run it off-peak.

## `storage`

```bash
record-store storage inspect --maximum-entries 100000 --endpoint <endpoint>
record-store storage repair  --maximum-entries 100000 --endpoint <endpoint>
record-store storage repair --apply --endpoint <endpoint>
```

`repair` is **dry-run unless `--apply` is given**. It removes orphaned payloads and
never removes files it does not recognise.

## Scripting

`--json` makes every command's output machine-readable:

```bash
#!/usr/bin/env bash
set -euo pipefail

export RECORD_STORE_MANAGEMENT_TOKEN=<your-management-token>
ENDPOINT=https://management.example.com

missing=$(record-store --json storage inspect --endpoint "$ENDPOINT" \
  | jq '.metadata_without_data')

if [ "$missing" -gt 0 ]; then
  echo "ALERT: $missing objects have missing payloads"
  exit 1
fi
```

Commands exit non-zero on failure, so `set -e` does the right thing.
