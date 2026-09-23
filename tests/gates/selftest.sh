#!/usr/bin/env bash
# Proves the gates can fail.
#
#   tests/gates/selftest.sh CANDIDATE_DIR
#
# A gate that has only ever passed is not yet evidence. This runs the evaluator's
# unit tests, then REC-CRASH with its negative control switched on -- a committed
# payload deleted while the server is down -- and requires the gate to FAIL
# with a product-failure exit code. It passes only if the gate catches the
# defect it exists to catch.
set -euo pipefail

candidate="${1:?usage: selftest.sh CANDIDATE_DIR}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 -m unittest discover -s "$here" -p 'test_*.py' -v

evidence="$(mktemp -d)"
trap 'rm -rf "$evidence"' EXIT
set +e
GATE_INJECT_DEFECT=delete-payload GATE_EVIDENCE_DIR="$evidence" \
  python3 "$here/crash_consistency.py" --bin-dir "$candidate/bin" --profile pr > "$evidence/output.log" 2>&1
status=$?
set -e
if [[ $status -ne 1 ]]; then
  echo "REC-CRASH did not fail on a deleted payload (exit $status); the gate is not detecting loss" >&2
  tail -40 "$evidence/output.log" >&2
  exit 1
fi
grep -q "FAIL\] .*every acknowledged object is readable" "$evidence/output.log" || {
  echo "REC-CRASH failed, but not on the check that should catch a lost payload" >&2
  exit 1
}
echo "self-test: REC-CRASH detects a lost payload"
