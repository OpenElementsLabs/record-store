#!/usr/bin/env bash
# Builds the candidate binaries and writes the manifest that identifies them.
#
#   tests/gates/build-candidate.sh OUT_DIR
#
# Every real-binary gate is given OUT_DIR/bin and records the digests it ran.
# The evaluator then refuses any result whose digests differ from the ones in
# OUT_DIR/candidate.json. That is what stops a gate from passing against a
# binary an earlier build left in target/release: the version string cannot,
# because unreleased work reports the previous release's version until the
# release bump.
#
# When CANDIDATE_BIN_DIR is set, nothing is compiled: the binaries there are
# adopted as the candidate (the release workflow passes the ones it extracted
# from the published image, so the gates test exactly what ships).
set -euo pipefail

out="${1:?usage: build-candidate.sh OUT_DIR}"
repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$out/bin"
out="$(cd "$out" && pwd)"

commit="$(git -C "$repository_root" rev-parse HEAD)"
if [[ -n "$(git -C "$repository_root" status --porcelain --untracked-files=no)" ]]; then
  tree_clean=false
else
  tree_clean=true
fi

if [[ -n "${CANDIDATE_BIN_DIR:-}" ]]; then
  source_kind="adopted"
  cp "$CANDIDATE_BIN_DIR/record-store" "$CANDIDATE_BIN_DIR/record-store-server" "$out/bin/"
else
  source_kind="built"
  # Both binaries, together, in one invocation: building only the server (as
  # the compatibility script does) leaves whatever CLI was there before.
  cargo build --manifest-path "$repository_root/Cargo.toml" --release --locked \
    --bin record-store-server --bin record-store
  cp "$repository_root/target/release/record-store" "$repository_root/target/release/record-store-server" "$out/bin/"
fi
chmod +x "$out/bin/record-store" "$out/bin/record-store-server"

sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

python3 - "$out/candidate.json" <<EOF
import json, platform, subprocess, sys
def run(*command):
    try:
        return subprocess.run(command, capture_output=True, text=True, check=True).stdout.strip()
    except Exception:
        return ""
json.dump({
    "schema": 1,
    "commit": "$commit",
    "tree_clean": $( [[ $tree_clean == true ]] && echo True || echo False ),
    "source": "$source_kind",
    "version": run("$out/bin/record-store", "--version"),
    "cargo_lock_sha256": "$(sha "$repository_root/Cargo.lock")",
    "rustc": run("rustc", "--version"),
    "platform": f"{platform.system().lower()}-{platform.machine()}",
    "binaries": {
        "record-store": "$(sha "$out/bin/record-store")",
        "record-store-server": "$(sha "$out/bin/record-store-server")",
    },
}, open(sys.argv[1], "w"), indent=2)
EOF
cat "$out/candidate.json"
