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
as usual. Close a finding by deleting its file
in the commit that fixes it: the gate's result is the proof, and history keeps
the record.

| Id | Title | Status | Blocks release |
| --- | --- | --- | --- |
