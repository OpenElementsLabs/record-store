# RSG-010: A crash can leave a publication record that stops the server starting

| | |
| --- | --- |
| Status | open |
| Severity | critical — one SIGKILL at the wrong moment takes the deployment down until an operator intervenes |
| Blocks release | yes |
| Gate | REC-CRASH |
| Known failing checks | `a publication record torn by a kill does not prevent start-up` |
| Found | 2026-09-23, candidate `417091a`, on the GitHub-hosted Linux runner (candidate profile, encrypted mode, first kill) |
| Modes | plaintext and encrypted |

After a SIGKILL, the next start failed:

```text
storage initialization failed: storage publication record encoding failed:
EOF while parsing a value at line 1 column 0
```

The durability page promises that an acknowledged write survives a process
crash. The data does survive; the process does not come back.

## Cause

`crates/record-store-storage/src/maintenance.rs`, `write_publication_record`,
creates `tmp/<id>.publish` under its final name (`create_new`) and only then
writes and syncs its contents, so a kill between the two leaves an empty (or,
after power loss, partial) record. At start-up,
`crates/record-store-storage/src/local_store.rs`, `recover_publications`, parses
every `.publish` file with `serde_json::from_slice(...)?` and aborts start-up on
the first one that does not parse. The crash-recovery journal is not itself
crash-atomic.

## Reproduction

Deterministic, in REC-CRASH on every run: stop a deployment, write an empty
`tmp/<uuid>.publish` — exactly what the kill window leaves — and start it.

```bash
out/venv/bin/python tests/gates/crash_consistency.py --bin-dir out/candidate/bin
# [FAIL] plaintext: a publication record torn by a kill does not prevent start-up
# [FAIL] encrypted: a publication record torn by a kill does not prevent start-up
```

Random kills reach the window only occasionally (once in ten kills in the first
Linux run, never in fourteen on the developer host), so until this is fixed
REC-CRASH will also fail intermittently with "exited with 1 before becoming
ready". That failure is not downgraded to a known failure: a start-up refusal
could have other causes.

## Direction of a fix (not applied here)

Write the record to a temporary name, fsync, rename into place, fsync the
directory; and let recovery treat a record that is empty or does not parse as
"publication never started" (the payload it would name was never committed),
removing it with a warning instead of refusing to start.
