# Release Gates

A release is shipped because the evidence says it may be, not because a
dashboard is green. The rules for that decision live in
[`release/gates.toml`](https://github.com/OpenElementsLabs/record-store/blob/main/release/gates.toml):
each gate states the guarantee it protects, the workload it runs, what it
measures and against what limit, where its evidence goes, when it runs, and
whether a failure blocks a release. CI runs gates by id from that file, so the
file is also what actually runs.

## Stages

| Stage | When | Adds |
| --- | --- | --- |
| `pr` | Every push and pull request | Format, Clippy, tests, skip accounting, console checks, dependency and secret scanning, fuzz smoke, documentation build |
| `integration` | Changes to code, lockfiles, packaging, tests or workflows | Real-binary gates: integrity on read, unsupported operations, crash recovery, backup and restore, upgrade from the previous release, S3 SDKs, console end-to-end, overload, secret redaction, artifact identity |
| `scheduled` | Weekly | Performance against a baseline, longer crash runs, 300 s fuzzing, 30-minute endurance |
| `candidate` | `workflow_dispatch` of the Gates workflow, a push to a `candidate/*` branch, and every release | Everything, against one candidate, plus packaging, provenance and both-architecture smoke tests in `release.yml` |

## What a gate result is bound to

`tests/gates/build-candidate.sh` builds both binaries and records the commit, a
clean-tree flag, the `Cargo.lock` digest, the toolchain and each binary's
SHA-256 in `candidate.json`. Every gate that starts a server reports the
digests of the binaries it actually ran, and a result counts toward a decision
only if:

- it was produced by the current definition of that gate — its entry in the
  matrix plus the files it lists as its implementation — so changing a gate
  invalidates its old results;
- it names the commit under evaluation, or the gate explicitly allows reuse and
  nothing it depends on changed since (performance and endurance only, with an
  age limit);
- for artifact-bound gates, it names the candidate's binary digests;
- it ran at least the workload profile the stage requires.

On a release, the artifact-bound gates run a second time against the binaries
extracted from the published image, so the evidence describes what ships.

## Outcomes

| Outcome | Meaning | Blocks |
| --- | --- | --- |
| `pass` | Every check held | — |
| `fail` | The product violated a guarantee, or the gate hung | Yes, if the gate is blocking |
| `invalid_measurement` | The run completed but proves nothing (for example, no kill landed mid-upload, or no baseline exists) | Yes |
| `infrastructure_error` | The gate could not run | Yes |
| `skipped`, `missing`, `stale` | No usable result | Yes |
| `known_failure` | In `pr` and `integration` only: every failed check matches an open finding recorded for that gate | Not a pull request; always a release |

Only infrastructure errors are retried, a bounded number of times, and every
attempt stays in the record. A product failure is never retried.

## Running gates yourself

```bash
tests/gates/run-stage.sh integration out/               # a whole stage
tests/gates/run-stage.sh integration out/ REC-CRASH     # one gate
tests/gates/selftest.sh out/candidate                   # prove the gates can fail
```

`out/report/report.md` is the decision; `out/report/report.json` is the
machine-readable form; `out/results/evidence/<gate>/` holds each gate's logs and
detailed checks. The real-binary gates generate their own credentials, bind
only loopback ports chosen at random, and work in temporary directories: they
never touch a running deployment.

## Skips, quarantine, exceptions and findings

- **A skipped test is not a pass.** Every `#[ignore]`, `.skip`, `t.Skip` and
  similar marker must appear in `release/quarantine.toml` with an owner, reason,
  issue, expiry and coverage risk. `.only` always fails.
- **Exceptions** (`release/exceptions/`) let one release ship past one failing
  gate. They are reviewed, complete, expiring records. Nothing creates or
  extends one automatically, and a release under exception says so in its
  decision.
- **Findings** (`release/findings/`) record defects the gates found, with a
  reproduction. An open finding marked as blocking release blocks every release
  decision until it is fixed or excepted; pull requests stay mergeable so the fix
  can land, but any failure the finding does not explain still blocks them.
  Fixing a gate so that it stops noticing a defect is not a fix.

## Thresholds

A threshold is either derived from a documented contract (the container start
period, the admission wait limit, the shutdown grace period), follows from the
design (memory must not grow with object size), or is labelled provisional.
Provisional limits are reported but do not block until they have been
calibrated. No latency or throughput target is set, because none is
documented. Performance is compared with the previous release, run on the same
machine in alternating rounds, using a noise-aware tolerance: hosted runners
differ from run to run, so a stored baseline would rarely describe the machine it
is compared on.
