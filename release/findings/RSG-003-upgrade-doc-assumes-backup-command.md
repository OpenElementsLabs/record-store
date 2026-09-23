# RSG-003: Upgrade guide asks 0.1.3 users for commands 0.1.3 does not have

| | |
| --- | --- |
| Status | fixed |
| Severity | medium — an operator following the guide may upgrade with no backup |
| Blocks release | no — fixed |
| Gate | CMP-UPGRADE, CMP-UPGRADE-CONTAINER |
| Found | 2026-09-23, candidate `0765aee` |

`docs/deployment/upgrading.md` step 1 is `record-store server backup` and
`verify-backup`, run before pulling the new image; rollback is
`record-store server restore`. 0.1.3 — the only supported starting point —
ships only `backup-metadata` and `restore-metadata`, which copy metadata and no
payloads. On 0.1.3 the documented commands fail as unknown subcommands.

CMP-UPGRADE shows the path that does work: run the **candidate's** CLI against
the stopped 0.1.3 data directory. That backup leaves the 0.1.3 directory
byte-identical, verifies at `full`, and restoring it lets 0.1.3 serve the
pre-upgrade state again. The guide and the release notes should say exactly that.

Second, the downgrade refusal: 0.1.3 started on an upgraded directory exits by
panicking inside redb 2.6.3 (`internal error: entered unreachable code`) rather
than with the schema message the guide implies. The directory is left unchanged
(asserted). The release notes should tell operators to expect that panic and
that it is harmless, rather than letting it read as corruption.

## Also found in the same section

- **The configuration check could never run.** Step 4 was
  `docker run ... record-store:<version> record-store server check-config`, but the
  image's entrypoint is already `record-store server`, so the process ran
  `record-store server record-store server check-config` and exited with
  `unrecognized subcommand 'record-store'` (reproduced with the candidate binary).
- **The backup came before the stop.** "The upgrade" ran `server backup` as step 1
  and `docker stop` as step 2; a backup refuses a running server (exit 6, asserted
  by REC-BACKUP), so step 1 always failed.

## Fix proposed in this change

`docs/deployment/upgrading.md` now stops first, backs up and verifies with the
new image, passes subcommands to the entrypoint correctly, explains the 0.1.3
case, and describes the downgrade panic. The binary-level behaviour behind each
step is exercised by CMP-UPGRADE and REC-BACKUP. The `docker run` wrappers
themselves (`--volumes-from`, the `/backups` ownership, the entrypoint
arguments) were not run: Docker was unavailable where this was written. Close
this finding after running the documented sequence once on a real 0.1.3
container deployment.

## Verified in containers

CMP-UPGRADE-CONTAINER followed the revised guide on the GitHub-hosted runner
with the published `ghcr.io/openelementslabs/record-store:0.1.3` and an image
built from `407199b`: all 23 checks passed, in the candidate run
(35887166104) and the integration run (35887161425). The downgrade attempt
exited with exactly the redb panic the guide now describes. The gate also fails
whenever the guide stops documenting a command form it runs, so the guide and
the verified procedure cannot drift apart unnoticed.
