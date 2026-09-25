<!--
Security fix? Do not open a public pull request. See SECURITY.md — a public
patch describes the flaw to everyone before deployments have it.
-->

## What this adds

<!-- What the change does, for somebody who will not read the diff. -->

## What it deliberately does not do

<!--
The scope you decided against, and why. A reviewer cannot recover this from the
diff, and it is usually the most useful sentence in the description.
-->

## Limits a user needs to know

<!--
Anything that constrains how this can be relied on: what a guarantee does not
cover, what happens on crash or restart, what is bounded, what is unbounded.
Write "none" if there genuinely are none.
-->

## Why it is built this way

<!--
Only where the approach is not obvious — a check placed in one layer rather than
another, a value refused rather than clamped, an option rejected. Delete if the
diff speaks for itself.
-->

---

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-features --locked`
- [ ] `bash tests/rust-audit.sh` — no new advisory or yanked crate
- [ ] `mkdocs build --strict`, if documentation changed
- [ ] `bash tests/compatibility/run.sh`, if the S3 surface changed
- [ ] `CHANGELOG.md` updated under `## [Unreleased]`, written for the person upgrading
- [ ] Documentation updated in this pull request, not a later one
- [ ] Tests fail without the change and pass with it
- [ ] No pinned test value was copied from a failing assertion's output
- [ ] No claim added that is stronger than what the code guarantees

<!--
Relates to #
-->
