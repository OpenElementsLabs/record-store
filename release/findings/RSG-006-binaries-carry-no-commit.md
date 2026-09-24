# RSG-006: Binaries carry no commit; unreleased builds report the previous version

| | |
| --- | --- |
| Status | open |
| Severity | low — traceability |
| Blocks release | no — a tagged build reports its version, which release.yml checks |
| Gate | ART-IDENTITY |
| Found | 2026-09-23, candidate `0765aee` |

`record-store --version`, `GET /api/v1/system/info` and a backup manifest's
`record_store_version` report `0.1.3` for the post-0.1.3 candidate, whose
catalog is schema 6 — while a real 0.1.3 writes schema 4. Nothing in a binary
names the commit it was built from, and `target/release/record-store` was
found a day older than the server binary beside it because
`tests/compatibility/run.sh` rebuilt only the server.

The gates therefore identify a candidate by the digests `build-candidate.sh`
records. Embedding the commit (a `build.rs` reading `GIT_COMMIT`, surfaced in
`--version`, `system/info` and the backup manifest) would let a deployment and a
backup name their build.
