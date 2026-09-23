# RSG-002: Release workflow published without the gates CI runs

| | |
| --- | --- |
| Status | fixed in this change |
| Severity | medium — gate infrastructure |
| Blocks release | no |
| Gate | release.yml (`gates` job) |
| Found | 2026-09-23, `0765aee` |

`release.yml` said it "runs the same gates CI runs", but ran only formatting,
Clippy, the Rust tests and the console checks before publishing images. The S3
SDK compatibility suite, the dependency audit, the console end-to-end suite and
the fuzz smoke run were not prerequisites of a release, and the published
image was smoke-tested on `linux/amd64` only although `linux/arm64` is shipped.
No real-binary recovery, crash, upgrade or redaction check existed at all.

Fixed by making the release call the gate workflow for the tag commit before
anything is published, running the real-binary gates again against the binaries
extracted from the published image, smoke-testing both architectures, and making
the GitHub Release depend on the evaluated report.
