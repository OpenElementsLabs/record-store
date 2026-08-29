# Testing

## What CI runs

| Job | Command |
| --- | --- |
| Format | `cargo fmt --all --check` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Test | `cargo test --workspace --all-features --locked` |
| Build | `cargo build --workspace --release --locked` |
| Console | `npm run format:check`, `lint`, `typecheck`, `test`, `build` |
| End-to-end | `npm run test:e2e`, `npm run test:e2e:cluster` |
| Compatibility | `tests/compatibility/run.sh` |
| Audit | `tests/rust-audit.sh` |

Run the first three before pushing. Clippy uses `-D warnings` — a warning is a failure.

## Rust tests

```bash
cargo test --workspace --all-features --locked

cargo test -p record-store-s3
cargo test -p record-store-api credential
cargo test -p record-store-core -- --nocapture
```

Unit tests are `#[cfg(test)] mod tests` in the same file as the code they cover.
Fixtures shared within a crate live in a `test_support` module.

## Integration tests

Tests that need a real binary are under `apps/*/tests/`. They spawn the process and
drive it.

`unsafe_code = "forbid"` means a test cannot call `std::env::set_var` — it is `unsafe`
in edition 2024. Tests that need specific environment variables run the binary as a
subprocess with `Command::env()` instead.

## Compatibility tests

Real AWS SDKs against a real server:

```bash
bash tests/compatibility/run.sh
```

The script builds the server, starts it on **dedicated ports** (47610, 47611, 47613),
runs each SDK's suite, and tears everything down.

Those ports are deliberate. Binding 7600 and 7601 would race whatever you already have
running, and because the readiness probe is just an HTTP call, the suite would happily
verify a foreign server and then test that instead of the binary it just built. If a
port is occupied, the script refuses to start rather than adopting an unknown service.

Override with `RECORD_STORE_COMPAT_S3_PORT`, `RECORD_STORE_COMPAT_API_PORT`, and
`RECORD_STORE_COMPAT_RPC_PORT`.

Pinned SDK versions:

| SDK | Version |
| --- | --- |
| `github.com/aws/aws-sdk-go-v2/service/s3` | 1.107.3 |
| `boto3` | 1.43.77 |
| `@aws-sdk/client-s3` | 3.1115.0 |
| `@aws-sdk/s3-request-presigner` | 3.1115.0 |

When adding an S3 feature, add a case here. A protocol test that passes against the
implementation's own assumptions proves less than one that passes against a real SDK.

## Console tests

```bash
cd console

npm test              # unit tests
npm run test:e2e      # Playwright, standalone
npm run test:e2e:cluster
```

The end-to-end suites build the console and run it against a real server. The cluster
suite runs a real multi-node cluster, not a mock.

```bash
npm run test:e2e:install    # first run only
```

## Consensus storage conformance

The Raft storage implementation is exercised by openraft's own `testing::Suite`. That
suite is the specification: if it passes, the storage layer satisfies the contract
openraft relies on. It is worth running whenever consensus storage changes.

## Dependency audit

```bash
tests/rust-audit.sh
```

Wraps `cargo audit` with one documented exception. The script first verifies the
exception is still safe — that the advisory's crate is genuinely not in the active
feature graph — and fails loudly if that ever changes.

That check is the point. A blanket `--ignore` would quietly stop being true.

## Benchmarks

```bash
cargo bench -p record-store-storage --bench storage
```

## Writing tests

**Name the behaviour, not the function.**

```rust
#[test]
fn a_rule_that_expires_nothing_is_refused() { }
```

reads better in a failure report than `test_validate_rule_error`.

**Say why the behaviour matters** when it is not obvious:

```rust
/// A rule that expires nothing would be silently inert. Requiring at least
/// one expiration is what stops an operator believing data is being cleaned
/// up when nothing is.
#[test]
fn a_rule_that_expires_nothing_is_refused() { }
```

**Pin behaviour you would be surprised to see change**, including behaviour you are not
sure is right — say so in the comment rather than leaving it untested.

**Assert on stable error codes**, not on status codes alone. Codes are the contract
clients branch on.

**Test the refusals.** Most of Record Store's security properties are things it declines
to do: a deny that overrides an allow, a decommission that is refused, an embed update
that would broaden access. Those need tests more than the happy paths do.

## Coverage

```bash
cargo llvm-cov --workspace --all-features
```

Coverage is a signal, not a target. A test that exercises a line without asserting
anything meaningful raises the number and catches nothing.
