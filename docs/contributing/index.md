# Contributing

Record Store is a Rust workspace plus a Next.js console.

<div class="grid cards" markdown>

-   **[Development Setup](development-setup.md)** — toolchain and running locally
-   **[Repository Structure](repository-structure.md)** — where things live
-   **[Testing](testing.md)** — what CI runs, and how to run it yourself
-   **[Releasing](releasing.md)** — cutting a version, and what a tag triggers

</div>

## Before opening a pull request

Run what CI runs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

And for the console:

```bash
cd console
npm run format:check
npm run lint
npm run typecheck
npm test
```

Clippy runs with `-D warnings`. A warning is a failure.

## Workspace conventions

| | |
| --- | --- |
| Edition | 2024 |
| Toolchain | Pinned to 1.97.1 by `rust-toolchain.toml` |
| `unsafe_code` | **`forbid`** at the workspace level |
| `dbg!`, `todo!`, `unimplemented!` | `deny` |

`unsafe_code = "forbid"` is not a preference — it cannot be overridden in a crate, so
anything requiring `unsafe` needs a different approach.

Because `todo!` and `unimplemented!` are denied, a partial implementation cannot be
merged behind a placeholder. Either it works or the code path does not exist.

## Reporting security issues

Report privately through the repository's security contact, not in a public issue.
