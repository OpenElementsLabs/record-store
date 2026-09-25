# Contributing to Record Store

Contributions are welcome: bug reports, documentation improvements, tests, and code.
Please follow our [Code of Conduct](CODE_OF_CONDUCT.md). Report security
vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Start here

1. Check existing issues and use the
   [issue forms](https://github.com/OpenElementsLabs/record-store/issues/new/choose)
   to report a problem or propose a change. Open an issue before starting work
   beyond a small fix.
2. Fork the repository and create a branch for your change.
3. Run the relevant checks below, then open a focused pull request describing the
   change, its limitations, and how you tested it. Link the related issue.

The current contribution scope excludes clustering, replication, erasure coding,
and new external services. Unsupported S3 operations must return an explicit error.

## Build and test

Use the Rust toolchain in [`rust-toolchain.toml`](rust-toolchain.toml). Console
work also requires Node.js 24. See
[Development Setup](https://openelementslabs.github.io/record-store/contributing/development-setup/)
for prerequisites, cloning, and running the server locally.

From the repository root:

```bash
cargo build --workspace --release --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

For console changes:

```bash
cd console
npm ci
npm run format:check
npm run lint
npm run typecheck
npm test
npm run build
```

The [Testing guide](https://openelementslabs.github.io/record-store/contributing/testing/)
covers additional checks, including dependency auditing, S3 client compatibility,
and end-to-end tests. For documentation changes, follow the
[documentation build instructions](https://openelementslabs.github.io/record-store/contributing/development-setup/#documentation)
and run `mkdocs build --strict`.

## Before opening a pull request

- Keep the change focused and add regression tests for fixes.
- Update affected documentation and `CHANGELOG.md` under `Unreleased`.
- Use a clear, imperative commit message and complete the pull request checklist.
- Describe guarantees and limitations accurately; all applicable CI checks must pass.

## Contributor guides

| Guide | What you will find |
| --- | --- |
| [Contributing overview](https://openelementslabs.github.io/record-store/contributing/) | Workspace conventions and contributor guidance |
| [Development Setup](https://openelementslabs.github.io/record-store/contributing/development-setup/) | Toolchains, local server, console, and documentation setup |
| [Testing](https://openelementslabs.github.io/record-store/contributing/testing/) | Test suites and CI checks |
| [Repository Structure](https://openelementslabs.github.io/record-store/contributing/repository-structure/) | Where code and supporting files live |
| [Releasing](https://openelementslabs.github.io/record-store/contributing/releasing/) | Release preparation and publishing |

Contributions are licensed under the project's [Apache License 2.0](LICENSE).
