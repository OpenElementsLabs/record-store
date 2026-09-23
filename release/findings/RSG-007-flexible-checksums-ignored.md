# RSG-007: CRC32/CRC32C/SHA-1 checksum headers are accepted and ignored

| | |
| --- | --- |
| Status | open |
| Severity | high — clients believe an integrity check happened |
| Blocks release | yes |
| Gate | CMP-UNSUPPORTED (`tests/gates/unsupported.py`) |
| Known failing checks | `x-amz-checksum-crc32 contradicts its body is not stored`; `x-amz-checksum-crc32c contradicts its body is not stored`; `x-amz-checksum-sha1 contradicts its body is not stored` |
| Found | 2026-09-23, candidate `0765aee` |

A PUT carrying `x-amz-checksum-crc32`, `x-amz-checksum-crc32c` or
`x-amz-checksum-sha1` whose value does not match the body returns `200` and
stores the body. `x-amz-checksum-sha256` is verified (`400 BadDigest`). Current
AWS SDKs compute CRC32 by default ("when supported") and send it as a header on
plain HTTP, so an application gets a success that it takes to mean the server
compared its checksum.

`docs/reference/s3-compatibility.md` declares only `x-amz-content-sha256`; the
flexible-checksum headers are outside the supported subset, and the contract
for anything outside it is an explicit refusal. Either verifying these headers
or refusing them with `NotImplemented` resolves the finding; storing a body that
contradicts one does not.

```bash
out/venv/bin/python tests/gates/unsupported.py --bin-dir out/candidate/bin
# [FAIL] a PUT whose x-amz-checksum-crc32 contradicts its body is not stored -- {'status': 200, 'stored': True}
```
