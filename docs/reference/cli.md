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

## `server`

Starts the server, or operates on its data offline.

```bash
record-store server --config /etc/record-store/config.toml
```

`--config` may also come from `RECORD_STORE_CONFIG_FILE`. With no config file, the
server runs on defaults plus environment variables.

### `server check-config`

```bash
record-store server check-config --config /etc/record-store/config.toml
```

Loads the file, applies the environment, validates, and exits. Binds nothing and writes
nothing.

### `server backup-metadata`

```bash
record-store server backup-metadata /backups/2026-08-29
```

Takes the exclusive data lock, so **the server must be stopped**. The destination must
not already exist.

### `server restore-metadata`

```bash
record-store server restore-metadata /backups/2026-08-29
```

Requires an empty `metadata/` directory and verifies every checksum. See
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
record-store bucket create <name> [--storage-class <class>] --endpoint <endpoint>
record-store bucket delete <name> --endpoint <endpoint>

record-store bucket versioning get <name> --endpoint <endpoint>
record-store bucket versioning enable <name> --endpoint <endpoint>
record-store bucket versioning suspend <name> --endpoint <endpoint>
```

`delete` requires the bucket to be empty. There is no `versioning disable` — see
[Versioning](../concepts/versioning.md).

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

## `cluster`

```bash
record-store cluster init --endpoint <endpoint>
record-store cluster status --endpoint <endpoint>
record-store cluster issue-join-token \
  --lifetime-seconds 3600 \
  --description "node-4" \
  --endpoint <endpoint>
```

`init` is idempotent: a cluster-mode server forms its initial consensus group before
accepting HTTP traffic, so this reports the cluster rather than creating a second one.

Join tokens are single-use, 60–86400 seconds.

## `node`

```bash
record-store node join --control <host:7603> --token <token> --config <file>
record-store node list --endpoint <endpoint>
record-store node inspect <id> --endpoint <endpoint>
record-store node drain <id> --endpoint <endpoint>
record-store node maintenance <id> --endpoint <endpoint>
record-store node resume <id> --endpoint <endpoint>
record-store node decommission <id> --force --endpoint <endpoint>
```

`join` takes an existing member's **RPC** address, not its management endpoint.
`--config` may come from `RECORD_STORE_CONFIG_FILE`.

`decommission` runs a durability safety check. `--force` bypasses the objection but
still moves the data. See [Node Lifecycle](../cluster/node-lifecycle.md).

## `placement`

```bash
record-store placement explain <bucket> <key> --endpoint <endpoint>

record-store placement simulate add-node --device-bytes <n> [--device-bytes <n>]...
    [--failure-domain <labels>] [--storage-class <class>] --endpoint <endpoint>
record-store placement simulate add-device <node> --usable-bytes <n>
    [--storage-class <class>] --endpoint <endpoint>
record-store placement simulate remove-device <node> <device> --endpoint <endpoint>
```

`simulate` changes nothing. It runs the real placement engine against a
hypothetical cluster map over a sample of committed placements, and reports what
would move.

The movement figure is **measured over that sample**, not extrapolated to a
duration: how long a migration takes depends on bandwidth Record Store has not
been told about. `placements_sampled` against `placements_total` says how much
of the cluster the answer is based on.

Runs the placement engine against committed state and changes nothing. Reports
the storage class and policy, the placement epoch, the failure domain in force,
every eligible device with its capacity weight and rendezvous score, the devices
that were selected, and every device that was **not** eligible with the rule that
ruled it out.

An object that does not exist yet is explained as the write that would create
it.

## `storage-class`

```bash
record-store storage-class list --endpoint <endpoint>
record-store storage-class show <class> --endpoint <endpoint>
record-store storage-class set <class> [--replicas N] [--failure-domain <scope>]
    [--strict] [--device-kind <kind>]... [--minimum-free-percent N]
    [--description <text>] --endpoint <endpoint>
record-store storage-class delete <class> --yes --endpoint <endpoint>
```

`--device-kind` is repeatable. Omitting it accepts any device kind.

`delete` is refused while devices still carry the class. See
[Storage Classes](../administration/storage-classes.md).

## `drive`

```bash
record-store drive list --endpoint <endpoint>
record-store drive discover --endpoint <endpoint>
record-store drive show <node> <device> --endpoint <endpoint>
record-store drive activate <node> <device> --endpoint <endpoint>
record-store drive drain <node> <device> --endpoint <endpoint>
record-store drive maintenance <node> <device> --endpoint <endpoint>
record-store drive resume <node> <device> --endpoint <endpoint>
record-store drive release <node> <device> --endpoint <endpoint>
record-store drive retire <node> <device> --yes --endpoint <endpoint>
```

`<device>` is the stable device identifier from `drive list`, not a path.

`discover` is read-only: it lists storage the node could use and registers
nothing. Devices are declared in configuration — see
[Storage Devices](../cluster/storage-devices.md).

`release` marks a device safe to remove and **fails** while it still holds
replicas, so success means evacuation finished rather than that it was
requested.

`retire` is permanent and prompts for confirmation. It refuses to run on a
non-interactive terminal unless `--yes` is passed. See
[Replacing a Drive](../cluster/replacing-a-drive.md).

## `repair`

```bash
record-store repair status --endpoint <endpoint>
```

## `rebalance`

```bash
record-store rebalance status --endpoint <endpoint>
record-store rebalance start --endpoint <endpoint>
```

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
