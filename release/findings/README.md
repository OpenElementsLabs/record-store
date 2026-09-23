# Findings

Defects and observations found by the release gates, one file each. A finding
is recorded with a reproduction and the gate that detects it; the gate keeps
failing until the defect is fixed or an exception is approved
(`../exceptions/`). Fixing the gate instead of the product is not a resolution.

The evaluator reads the header table of every file here. A finding whose
`Status` is `open` and whose `Blocks release` is `yes` blocks every release
decision -- the candidate stage and release.yml's pre-publish run -- until it is
closed or excepted.

Pull requests are judged differently, so that the fixes can land: in the `pr`
and `integration` stages a failing gate is reported as a *known failure* when
every one of its failed checks matches a `Known failing checks` pattern of an
open finding for that gate. A single new failed check in the same gate blocks
as usual. Close a finding by setting
`Status` to `fixed` with the commit that fixed it; the gate's result is the
proof.

| Id | Title | Status | Blocks release |
| --- | --- | --- | --- |
| [RSG-001](RSG-001-corrupted-plaintext-read-served.md) | Plaintext whole-object read serves corrupted bytes with 200 | open | yes |
| [RSG-002](RSG-002-release-workflow-skipped-gates.md) | Release workflow published without the gates CI runs | fixed in this change | no |
| [RSG-003](RSG-003-upgrade-doc-assumes-backup-command.md) | Upgrade guide: commands 0.1.3 lacks, a check that cannot run, backup before stop | open (fix proposed, unverified in a container) | yes |
| [RSG-004](RSG-004-no-header-read-timeout.md) | No header read timeout: slow clients hold connections indefinitely | open | no |
| [RSG-005](RSG-005-small-writes-serialize.md) | Small-write throughput does not scale with concurrency | open (observation) | no |
| [RSG-006](RSG-006-binaries-carry-no-commit.md) | Binaries carry no commit; unreleased builds report the previous version | open | no |
| [RSG-007](RSG-007-flexible-checksums-ignored.md) | CRC32/CRC32C/SHA-1 checksum headers are accepted and ignored | open | yes |
