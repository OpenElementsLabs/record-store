# Testing

## What CI runs

| Job | Command |
| --- | --- |
| Format | `cargo fmt --all --check` |
| Lint | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Test | `cargo test --workspace --all-features --locked` |
| Build | `cargo build --workspace --release --locked` |
| Console | `npm run format:check`, `lint`, `typecheck`, `test`, `build` |
| End-to-end | `npm run test:e2e` |
| Compatibility | `tests/compatibility/run.sh` |
| Audit | `tests/rust-audit.sh` |
| Fuzz | `tests/fuzz-smoke.sh` |

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
npm run test:e2e      # Playwright
```

The end-to-end suite builds the console and runs it against a real server, not a mock.

```bash
npm run test:e2e:install    # first run only
```

## Dependency audit

```bash
tests/rust-audit.sh
```

Runs `cargo audit --deny warnings` over `Cargo.lock`. There are no exceptions, and
adding one should be the last resort rather than the first: an `--ignore` is a claim
that stops being checked the moment it is written.

`--deny warnings` is what makes a yanked crate a failure. A yank is not yet an
advisory, but it is the crate's author saying the build should not be used, and it is
far cheaper to move off one now than after it becomes an advisory.

Note that a crate can appear in `Cargo.lock` without being compiled — an optional
dependency of a dependency is recorded there whether or not the feature enabling it
is on. `cargo audit` reads the lockfile, so it flags those too. `cargo tree -i
<crate> --target all` printing nothing means the crate is not in the build, which
tells you an upgrade is housekeeping rather than an exposure; it does not make the
finding something to suppress.

## Fuzzing

```bash
cargo install cargo-fuzz --locked
rustup toolchain install nightly

tests/fuzz-smoke.sh                 # every target, 20s each
FUZZ_SECONDS=300 tests/fuzz-smoke.sh

cd fuzz && cargo +nightly fuzz run s3_xml_documents
```

The targets live in [`fuzz/`](https://github.com/OpenElementsLabs/record-store/tree/main/fuzz),
a workspace of their own. They have to be: `cargo fuzz` builds with sanitizers and a
nightly-only instrumentation pass, and the generated entry point is `unsafe`, which
the root workspace forbids.

Each target covers a parser that runs **before a request is authenticated** — the XML
request bodies, the `Authorization` header, the presigned-URL query, the `Range`
header, the ListObjectsV2 query, and the bucket-name and object-key validators. Those
are the places where the bytes are chosen entirely by an anonymous caller, which is
what makes them worth the machine time.

| Target | Covers |
| --- | --- |
| `s3_xml_documents` | `CORSConfiguration`, `VersioningConfiguration`, `CompleteMultipartUpload` bodies |
| `sigv4_authorization` | `Authorization` and `X-Amz-Date` headers |
| `sigv4_canonical_query` | Presigned-URL parameters and query canonicalisation |
| `s3_range_header` | `Range` resolved against an object size |
| `s3_list_query` | ListObjectsV2 parameters and percent-decoding |
| `core_names` | Bucket-name and object-key validation |
| `core_cors_patterns` | CORS origin and header pattern matching |

The S3 parsers are private to their crate. `record-store-s3` exposes them through
`src/fuzzing.rs` behind a `fuzzing` feature that no shipping build enables — a narrow
window for the targets rather than a widening of the crate's API.

**Assert an invariant, not just the absence of a panic.** A target that only calls a
parser finds crashes; a target that says what must be true of an accepted input finds
wrong answers, which is the larger category. `s3_range_header` asserts that an
accepted range lies inside the object. `core_names` asserts that an accepted key
contains no `..`, no empty segment, and no backslash — the property every layer below
it is written to rely on. Put the assertion in the target, next to the call, so that
changing the parser and changing what is claimed about it land in the same diff.

**Be sure the invariant is actually the parser's job.** Part numbers are range-checked
in the handler, not in the deserialiser, so a target that stopped at `quick_xml` and
then asserted `1..=10000` would report a bug that is not one. Where validation is
split like that, the wrapper in `src/fuzzing.rs` mirrors the handler.

CI runs each target for twenty seconds. That is not a search — it is what stops a
renamed parser from leaving behind a harness that still compiles and reaches nothing.
Finding something new means running one for hours against a corpus kept between runs.

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
to do: a deny that overrides an allow, a quota that is enforced, an embed update
that would broaden access. Those need tests more than the happy paths do.

## Coverage

```bash
cargo llvm-cov --workspace --all-features
```

Coverage is a signal, not a target. A test that exercises a line without asserting
anything meaningful raises the number and catches nothing.
