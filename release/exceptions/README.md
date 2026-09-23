# Release-gate exceptions

An exception lets one candidate ship while one blocking gate is not passing. It
is a reviewed, expiring record, never a flag: the evaluator reads the files in
this directory, and nothing ever writes them except a person in a pull request.

A gate is waived only when an exception file names it, carries every field
below, has not expired and, if `candidate_commit` is set, names the commit being
evaluated. An incomplete file is an error (the evaluation exits 2), not a
waiver. A waived gate is reported as `excepted` in every report, alongside the
exception's owner and expiry, and the decision reads `READY WITH EXCEPTIONS`.

```toml
# release/exceptions/EX-2026-001.toml
id = "EX-2026-001"
gate = "COR-INTEGRITY"
guarantee = "What the gate protects that will not be verified for this release"
evidence = "Where the failing result and its reproduction are (release/findings/..., run URL)"
risk = "Who is exposed, how, how likely, and what mitigates it until it is fixed"
owner = "github-handle responsible for the fix"
approved_by = "github-handle of the maintainer who accepted the risk"
created = 2026-09-23
expires = 2026-10-07          # short: an exception is a bridge, not a policy
candidate_commit = "..."      # optional: limit the waiver to one candidate
```

There are no exceptions at matrix `2026.09.23-1`.
