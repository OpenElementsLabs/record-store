#!/usr/bin/env bash
set -euo pipefail

# rust_decimal declares rkyv 0.7 as an optional backend, so Cargo.lock records
# it even though Record Store's byte-unit -> openraft feature graph does not enable it.
# Fail before applying the narrow audit exception if that ever changes.
active_rkyv="$(cargo tree -e features -i rkyv@0.7.46 2>/dev/null || true)"
if [[ -n "$active_rkyv" ]]; then
  echo "RUSTSEC-2026-0235 exception is no longer safe: rkyv is active" >&2
  echo "$active_rkyv" >&2
  exit 1
fi

cargo audit --ignore RUSTSEC-2026-0235
