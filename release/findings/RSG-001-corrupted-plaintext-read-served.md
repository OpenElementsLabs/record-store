# RSG-001: Plaintext whole-object read serves corrupted bytes with 200

| | |
| --- | --- |
| Status | open |
| Severity | high — silent data corruption reaches clients |
| Blocks release | yes |
| Gate | COR-INTEGRITY (`tests/gates/integrity_read.py`) |
| Known failing checks | `plaintext: whole read of a payload with one flipped byte fails visibly` |
| Found | 2026-09-23, candidate `0765aee` |
| Modes | standalone, plaintext payloads (encrypted payloads are not affected) |

## What the contract says

`docs/concepts/durability.md`: a whole-object read has its "SHA-256 recomputed
while streaming. A mismatch fails the read before its last chunk." — and "the
guarantee is that the read *fails* rather than completing silently."

## What happens

A same-length change to a stored plaintext payload is served as a successful
read: HTTP 200, the full `Content-Length`, the corrupted bytes, and a cleanly
closed response. Observed at 100 B, 1000 B, 5000 B, 70 KB, 300 KB, 3 MiB and
20 MiB. Truncated and missing payloads are handled correctly, as is every
encrypted case, and `POST /api/v1/verify/buckets/{bucket}` counts the damage.

## Reproduction

```bash
tests/gates/build-candidate.sh out/candidate
python3 -m venv out/venv && out/venv/bin/pip install -r tests/gates/requirements.txt
out/venv/bin/python tests/gates/integrity_read.py --bin-dir out/candidate/bin
# [FAIL] plaintext: whole read of a payload with one flipped byte fails visibly (single-chunk)
#   {'status': 200, 'declared': 1000, 'received': 1000, 'broken': False, 'matches_original': False}
```

By hand: PUT an object, stop the server, flip one byte in its file under
`objects/`, start the server, `curl` a presigned GET: `200`, exit 0, wrong bytes.

## Cause

`crates/record-store-storage/src/integrity.rs`, `verifying_stream`: every data
chunk is yielded as it is hashed, and the digest comparison is chained *after*
the last one. By the time `IntegrityMismatch` is produced, all
`Content-Length` bytes have been written and the client has a complete,
well-formed response. The existing unit tests assert that the stream yields an
error, not what an HTTP client receives.

## Direction of a fix (not applied here)

Hold back the final chunk until the digest over everything before it plus that
chunk matches, then release it; on mismatch, abort the connection so the client
sees a short body. The added latency is one chunk. The regression test belongs
at the HTTP level, where COR-INTEGRITY already measures it.
