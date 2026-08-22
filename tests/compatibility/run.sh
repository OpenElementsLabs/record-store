#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
compatibility_root="$repository_root/tests/compatibility"
run_directory="$(mktemp -d "${TMPDIR:-/tmp}/oes-compat.XXXXXXXX")"
server_pid=""

cleanup() {
  result=$?
  trap - EXIT
  if [[ -n "$server_pid" ]]; then
    kill -TERM "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ $result -ne 0 && -f "$run_directory/server.log" ]]; then
    tail -100 "$run_directory/server.log" >&2
  fi
  rm -rf "$run_directory"
  exit "$result"
}
trap cleanup EXIT

# Dedicated ports, not the production ones. Binding 7600/7601 here would make
# the suite race whatever the developer already has running, and because the
# readiness probe is just an HTTP call it would happily verify a foreign server
# and then test that instead of the binary just built.
s3_port="${OES_COMPAT_S3_PORT:-47610}"
api_port="${OES_COMPAT_API_PORT:-47611}"
rpc_port="${OES_COMPAT_RPC_PORT:-47613}"
management_token="oes-compat-management-token-at-least-thirty-two-bytes"

for entry in "S3:$s3_port:OES_COMPAT_S3_PORT" "management:$api_port:OES_COMPAT_API_PORT" \
  "RPC:$rpc_port:OES_COMPAT_RPC_PORT"; do
  label="${entry%%:*}"
  rest="${entry#*:}"
  candidate="${rest%%:*}"
  variable="${rest#*:}"
  if ! python3 -c "
import socket, sys
probe = socket.socket()
probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    probe.bind(('127.0.0.1', int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    probe.close()
" "$candidate"; then
    echo "$label compatibility port 127.0.0.1:$candidate is occupied; refusing to adopt an unknown service. Set $variable to a free port." >&2
    exit 1
  fi
done

export OES_ROOT_ACCESS_KEY="oes-compat-root"
export OES_ROOT_SECRET_KEY="oes-compat-root-secret-at-least-sixteen"
export OES_CREDENTIAL_MASTER_KEY="oes-compat-stable-master-key-at-least-thirty-two-bytes"
export OES_MANAGEMENT_SYSTEM_TOKEN="$management_token"
export OES_STORAGE_DATA_DIRECTORY="$run_directory/data"
export OES_STORAGE_ENCRYPTION_ENABLED="true"
export OES_MODE="standalone"
export OES_S3_BIND="127.0.0.1:$s3_port"
export OES_API_BIND="127.0.0.1:$api_port"
export OES_RPC_BIND="127.0.0.1:$rpc_port"
export OES_COMPAT_ENDPOINT="http://127.0.0.1:$s3_port"
export AWS_REQUEST_CHECKSUM_CALCULATION="WHEN_REQUIRED"
export AWS_RESPONSE_CHECKSUM_VALIDATION="WHEN_REQUIRED"

cargo build --manifest-path "$repository_root/Cargo.toml" --bin oes-server --release --locked
"$repository_root/target/release/oes-server" >"$run_directory/server.log" 2>&1 &
server_pid=$!

# Liveness is checked before readiness: a dead child with a reachable port means
# something else is answering, which must fail rather than be tested against.
for _ in {1..100}; do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "OES exited before becoming ready" >&2
    exit 1
  fi
  if curl --fail --silent "http://127.0.0.1:$api_port/ready" >/dev/null; then
    break
  fi
  sleep 0.1
done
kill -0 "$server_pid" 2>/dev/null || { echo "OES exited before becoming ready" >&2; exit 1; }
curl --fail --silent "http://127.0.0.1:$api_port/ready" >/dev/null

# Confirm the server answering is the one just started, in the mode expected.
identity="$(curl --fail --silent -H "authorization: Bearer $management_token" \
  "http://127.0.0.1:$api_port/api/v1/system/info")"
case "$identity" in
  *'"name":"oes"'*'"mode":"standalone"'*) ;;
  *)
    echo "unexpected backend on 127.0.0.1:$api_port: $identity" >&2
    exit 1
    ;;
esac

python3 -m venv "$run_directory/venv"
"$run_directory/venv/bin/pip" install --quiet --requirement "$compatibility_root/requirements.txt"
"$run_directory/venv/bin/python" "$compatibility_root/boto3_compat.py"

mkdir "$run_directory/javascript"
cp "$compatibility_root/javascript/package.json" "$compatibility_root/javascript/package-lock.json" \
  "$compatibility_root/javascript/compat.mjs" "$run_directory/javascript/"
npm ci --silent --ignore-scripts --prefix "$run_directory/javascript"
node "$run_directory/javascript/compat.mjs"

(cd "$compatibility_root/go" && go test -count=1 ./...)
