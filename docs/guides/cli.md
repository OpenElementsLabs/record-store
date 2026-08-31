# Command Line

`record-store` is the operational CLI. It runs the server and administers a running
deployment through the management API.

For the full command tree, see the [CLI Reference](../reference/cli.md).

## Authentication

Every management command reads its credential from the environment:

```bash
export RECORD_STORE_MANAGEMENT_TOKEN="$RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN"
```

That may be a system, storage, or auditor token — the role decides what the command is
allowed to do. See [Authorization](../security/authorization.md).

If no system token is configured, root Basic authentication remains available for
development compatibility using `RECORD_STORE_ROOT_ACCESS_KEY` and
`RECORD_STORE_ROOT_SECRET_KEY`. Record Store emits a warning when it does this.

## Endpoint

Commands default to `http://127.0.0.1:7601`. Override per command:

```bash
record-store bucket list --endpoint https://record-store.internal:7601
```

## Running the server

```bash
record-store server                          # start
record-store server --config ./record-store.toml
record-store server check-config             # validate and exit
```

`check-config` loads the file and environment, validates everything, and exits without
binding listeners. Use it in CI and before a restart.

## Everyday administration

```bash
record-store status
record-store bucket list
record-store bucket create demo
record-store bucket versioning enable demo
record-store service-account create my-app
record-store credential rotate <account-id>
record-store policy create ./policy.json
record-store policy attach <policy-id> <account-id>
record-store audit --limit 100
record-store verify object demo reports/q1.pdf
record-store storage inspect
```

## JSON output

Every command accepts `--json` for automation:

```bash
record-store bucket list --json | jq -r '.[].name'
```

## Storage repair is a dry run by default

```bash
record-store storage inspect            # report only
record-store storage repair             # dry run, reports what it would remove
record-store storage repair --apply     # actually delete orphaned payloads
```

!!! warning "`--apply` deletes data"
    Inspect the dry-run output before applying. Repair only removes payloads the
    catalog no longer references, but read
    [Integrity Verification](../operations/integrity-verification.md) first.

## Offline backup

```bash
record-store server backup-metadata ./backup-2026-08-29
record-store server restore-metadata ./backup-2026-08-29
```

!!! danger "Stop the server first"
    These take an exclusive lock on the data directory and refuse to race a running
    server. They back up **metadata only** — not object payloads. See
    [Backup and Restore](../operations/backup-and-restore.md).

## In a container

The CLI ships in the same image as the server:

```bash
docker compose exec \
  -e RECORD_STORE_MANAGEMENT_TOKEN=<token> \
  record-store record-store bucket list
```

Pass the token with `-e`. A shell variable on your host does not reach the container.
