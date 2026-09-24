# Release gates

This directory holds the rules and records behind every release decision. The
public explanation is `docs/contributing/release-gates.md`; this file is the
maintainer's map.

| Path | What it is | Who changes it |
| --- | --- | --- |
| `gates.toml` | The versioned standalone gate matrix: guarantee, command, fixtures, failure injection, thresholds and their basis, evidence, stages, blocking, reuse, limitations | A reviewed pull request. Bump `matrix_version` when semantics change |
| `quarantine.toml` | Every skipped test, with owner, reason, issue, expiry, coverage risk | A reviewed pull request; entries expire |
| `exceptions/` | Waivers for one gate on one release | A person, in a pull request; never the tooling |
| `findings/` | Defects the gates found, with reproductions; open blockers block | Whoever fixes or triages the defect |
| `public-docs-allowlist.txt` | Reviewed public-docs lines that mention clustering-related terms | A reviewed pull request |

The implementation is in `tests/gates/`:

| File | Role |
| --- | --- |
| `build-candidate.sh` | Builds (or adopts) the candidate and writes `candidate.json`: commit, tree state, `Cargo.lock` digest, toolchain, binary digests |
| `record.py` | Runs one gate by id from the matrix; classifies the outcome; bounded retries for infrastructure errors only; binds the record to commit, definition digest, profile and binary digests |
| `evaluate.py` | Turns records into `report.json` and `report.md`; MISSING, STALE, SKIPPED, INVALID and INFRA all block; applies exceptions, quarantine and findings |
| `run-stage.sh` | Runs a stage (or named gates) locally or in CI |
| `gatelib.py` | The real-binary harness: isolated servers, generated credentials, random loopback ports, resource sampling |
| `selftest.sh`, `test_evaluate.py` | Prove the gates and the evaluator fail when they should |
| One script per real-binary gate | `crash_consistency.py`, `integrity_read.py`, `recovery_drill.py`, `upgrade.py`, `unsupported.py`, `overload.py`, `redaction.py`, `perf.py`, and the repository checks |

Internal cluster readiness has its own matrix, `internal/cluster/gates.toml`,
evaluated separately. It is excluded from the documentation build.

## Evidence and retention

| Evidence | Where | Kept |
| --- | --- | --- |
| Per-gate records and evidence (logs, `detail.json`) | `<prefix>-results-*` workflow artifacts | 90 days |
| Stage decision (`report.json`, `report.md`), and the internal cluster report | `<prefix>-report` artifact; the public step summary shows the standalone report only | 400 days |
| Candidate binaries and `candidate.json` | `<prefix>-candidate` artifact | 30 days |
| The decision a release shipped under | Release assets `record-store-<version>-release-gates.{json,md}`, covered by `SHA256SUMS` | As long as the release |

PERF-BASELINE, PERF-ENDURANCE and PERF-OVERLOAD run only in the weekly scheduled stage; they
do not block a candidate or a release. PERF-BASELINE compares the build with the
previous release on the same runner rather than with a stored baseline: GitHub-hosted runners do not all have
the same CPU, so a stored number would rarely describe the machine it is
compared on. When a release ships, the workflows' previous-release reference
(`v0.1.3` today) moves to it.

## Negative controls demonstrated at matrix 2026.09.23-2

A gate that has never failed is not yet evidence. Each of these was observed to
fail on a real defect or an injected one:

| Gate | Shown to fail on |
| --- | --- |
| REC-CRASH | A committed payload deleted, or one byte flipped, while the server was down (`GATE_INJECT_DEFECT`); runs on every change via GATES-SELFTEST |
| REC-CRASH | A run whose kills never landed mid-upload: reported `invalid_measurement`, not pass |
| REC-CRASH | The live product defect RSG-010, deterministically (an empty publication record) |
| COR-PAGINATION | The live product defect RSG-008, at every page size |
| COR-INTEGRITY | The live product defect RSG-001 |
| CMP-UNSUPPORTED | The live product defect RSG-007 (and a malformed-value variant, removed so a refusal cannot be a parse error) |
| CL-EVIDENCE | A failure-matrix row citing a test that does not exist |
| COR-SKIPS | `#[ignore]`, `.skip`, `.fixme`, `t.Skip`, `pytest.skip`, `@Disabled`, `.only` patterns |
| PERF-BASELINE | No baseline for the environment: `invalid_measurement` |
| record.py | An artifact-bound gate that did not report the binary it ran: `invalid_measurement` |
| evaluate.py | 20 unit tests: missing, skipped, stale, lighter profile, other binaries, infrastructure, invalid, expired or incomplete exceptions, expired quarantine, open findings, known failures, candidate-commit mismatch, malformed matrix |
