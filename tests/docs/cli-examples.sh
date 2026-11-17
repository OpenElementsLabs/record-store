#!/usr/bin/env bash
# Runs the AWS CLI examples from the documentation against a freshly built
# Record Store, from an empty home directory, and reports which ones fail.
#
# The profile is created by executing the `aws configure set` lines from
# docs/.snippets/aws-cli-profile.md as written, so a broken snippet fails here.
# Every other command is copied from the page named next to it; when you change
# a command in the docs, change it here too.
#
# Usage:
#   bash tests/docs/cli-examples.sh
#
# Environment:
#   AWS_CLI                       AWS CLI command to test (default: aws)
#   RECORD_STORE_DOCS_BIN_DIR     directory holding prebuilt record-store and
#                                 record-store-server binaries (default: build)
#   RECORD_STORE_DOCS_S3_PORT     S3 port (default 47620)
#   RECORD_STORE_DOCS_API_PORT    management port (default 47621)
#   RECORD_STORE_DOCS_RPC_PORT    RPC port (default 47623)
#
# Not covered here, because they need TLS or a hostname endpoint: the
# aws-chunked checksum failure (clients send checksums as headers over plain
# HTTP), and virtual-hosted versus path-style addressing (IP endpoints always
# use path-style). See the summary printed at the end.
set -uo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
snippet="$repository_root/docs/.snippets/aws-cli-profile.md"
aws_cli_guide="$repository_root/docs/guides/aws-cli.md"
run_directory="$(mktemp -d "${TMPDIR:-/tmp}/record-store-docs.XXXXXXXX")"
server_pid=""

cleanup() {
  result=$?
  trap - EXIT
  if [[ -n "$server_pid" ]]; then
    kill -TERM "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  if [[ $result -ne 0 && -f "$run_directory/server.log" ]]; then
    echo "--- last 50 lines of the server log" >&2
    tail -50 "$run_directory/server.log" >&2
  fi
  rm -rf "$run_directory"
  exit "$result"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Reporting

passed=0
failed=0
failures=()

pass() { passed=$((passed + 1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() {
  failed=$((failed + 1)); failures+=("$1")
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  [[ -n "${2:-}" ]] && printf '%s\n' "$2" | sed 's/^/       /' | head -15
}
section() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# check NAME COMMAND...           the command must succeed
check() {
  local name="$1"; shift
  local output
  if output="$("$@" 2>&1)"; then pass "$name"; else fail "$name" "$output"; fi
}

# check_fails NAME TEXT COMMAND...  the command must fail, with TEXT in its output
check_fails() {
  local name="$1" expected="$2"; shift 2
  local output
  if output="$("$@" 2>&1)"; then
    fail "$name" "expected a failure containing '$expected', but the command succeeded"
  elif [[ "$output" == *"$expected"* ]]; then
    pass "$name"
  else
    fail "$name" "expected '$expected' in the output, got:"$'\n'"$output"
  fi
}

json_field() { python3 -I -c 'import json,sys
value = json.load(sys.stdin)
for part in sys.argv[1].split("."):
    value = value[int(part)] if isinstance(value, list) else value[part]
print(value)' "$1"; }

# ---------------------------------------------------------------------------
# Start a server on dedicated ports, as tests/compatibility/run.sh does

s3_port="${RECORD_STORE_DOCS_S3_PORT:-47620}"
api_port="${RECORD_STORE_DOCS_API_PORT:-47621}"
rpc_port="${RECORD_STORE_DOCS_RPC_PORT:-47623}"
for port in "$s3_port" "$api_port" "$rpc_port"; do
  if ! python3 -I -c '
import socket, sys
probe = socket.socket()
probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    probe.bind(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    probe.close()' "$port"; then
    echo "port 127.0.0.1:$port is occupied; set RECORD_STORE_DOCS_*_PORT to a free port" >&2
    exit 1
  fi
done

root_access_key="record-store-docs-root"
root_secret_key="record-store-docs-root-secret-at-least-sixteen"
management_token="record-store-docs-management-token-at-least-thirty-two-bytes"
s3_endpoint="http://127.0.0.1:$s3_port"
api_endpoint="http://127.0.0.1:$api_port"

if [[ -n "${RECORD_STORE_DOCS_BIN_DIR:-}" ]]; then
  bin_dir="$RECORD_STORE_DOCS_BIN_DIR"
else
  cargo build --manifest-path "$repository_root/Cargo.toml" --release --locked \
    --bin record-store-server --bin record-store || exit 1
  bin_dir="$repository_root/target/release"
fi

(
  cd "$run_directory" || exit 1
  export RECORD_STORE_ROOT_ACCESS_KEY="$root_access_key"
  export RECORD_STORE_ROOT_SECRET_KEY="$root_secret_key"
  export RECORD_STORE_CREDENTIAL_MASTER_KEY="record-store-docs-stable-master-key-at-least-thirty-two-bytes"
  export RECORD_STORE_MANAGEMENT_SYSTEM_TOKEN="$management_token"
  export RECORD_STORE_STORAGE_DATA_DIRECTORY="$run_directory/data"
  export RECORD_STORE_MODE="standalone"
  export RECORD_STORE_S3_BIND="127.0.0.1:$s3_port"
  export RECORD_STORE_API_BIND="127.0.0.1:$api_port"
  export RECORD_STORE_RPC_BIND="127.0.0.1:$rpc_port"
  exec "$bin_dir/record-store-server"
) >"$run_directory/server.log" 2>&1 &
server_pid=$!

for _ in {1..100}; do
  kill -0 "$server_pid" 2>/dev/null || { echo "Record Store exited before becoming ready" >&2; exit 1; }
  curl --fail --silent "$api_endpoint/ready" >/dev/null && break
  sleep 0.1
done
curl --fail --silent "$api_endpoint/ready" >/dev/null || { echo "Record Store did not become ready" >&2; exit 1; }

# The management CLI, as the docs show it: token in the environment, --endpoint
# after the subcommand.
rs_admin() { RECORD_STORE_MANAGEMENT_TOKEN="$management_token" "$bin_dir/record-store" "$@"; }

# ---------------------------------------------------------------------------
# An empty home directory and no AWS variables, so nothing outside the docs can
# supply a setting the docs forgot.

# Resolve the binary before defining the aws() wrapper below, which would
# otherwise call itself.
aws_bin="$(command -v "${AWS_CLI:-aws}")" || { echo "AWS CLI not found: ${AWS_CLI:-aws}" >&2; exit 1; }
export HOME="$run_directory/home"
mkdir -p "$HOME"
cd "$run_directory/home" || exit 1
for variable in $(env | sed -n 's/^\(AWS_[A-Z_]*\)=.*/\1/p'); do
  [[ "$variable" == "AWS_CLI" ]] || unset "$variable"
done
aws() { $aws_bin "$@"; }

echo "Record Store: $s3_endpoint (S3), $api_endpoint (management)"
echo "AWS CLI:      $(aws --version 2>&1)"
echo "Home:         $HOME (empty)"

# ---------------------------------------------------------------------------
section "Profile setup (docs/.snippets/aws-cli-profile.md, docs/guides/aws-cli.md)"

snippet_commands="$(sed -n 's/^[[:space:]]*\(aws configure set .*\)$/\1/p' "$snippet")"
guide_commands="$(awk '/^## Configure a profile/{on=1} /^## Common operations/{on=0} on' "$aws_cli_guide" \
  | sed -n 's/^[[:space:]]*\(aws configure set .*\)$/\1/p')"
if [[ -n "$snippet_commands" && "$snippet_commands" == "$guide_commands" ]]; then
  pass "the snippet and the AWS CLI guide show the same profile commands"
else
  fail "the snippet and the AWS CLI guide show different profile commands" \
    "$(diff <(echo "$snippet_commands") <(echo "$guide_commands"))"
fi

profile_ok=1
while IFS= read -r line; do
  line="${line//ENDPOINT/$s3_endpoint}"
  line="${line//SECRET_KEY/$root_secret_key}"
  line="${line//ACCESS_KEY/$root_access_key}"
  read -r -a words <<<"$line"
  if ! output="$(aws "${words[@]:1}" 2>&1)"; then
    profile_ok=0; fail "profile command: $line" "$output"
  fi
done <<<"$snippet_commands"
[[ $profile_ok -eq 1 ]] && pass "ran the $(wc -l <<<"$snippet_commands" | tr -d ' ') profile commands from the snippet"

check "aws --profile record-store s3api list-buckets" aws --profile record-store s3api list-buckets

# Operations where S3 requires a checksum (bucket versioning, CORS, Object Lock
# settings) are signed with the CLI's default algorithm whatever the profile
# says. Recent CLI versions choose CRC64NVME, which Record Store refuses with
# NotImplemented, so the docs pass --checksum-algorithm CRC32 to those commands
# (docs/guides/aws-cli.md, "Commands that require a checksum").
required_algorithm="$(aws --profile record-store s3api put-bucket-versioning --bucket algorithm-probe \
  --versioning-configuration Status=Enabled --debug 2>&1 \
  | grep -oE "'x-amz-sdk-checksum-algorithm': '[A-Z0-9]+'" | head -1 | grep -oE "[A-Z0-9]+'$" | tr -d "'")"
echo "  info: this CLI signs checksum-required operations with ${required_algorithm:-no checksum header}"
if [[ "$required_algorithm" == "CRC64NVME" ]]; then
  check_fails "without --checksum-algorithm CRC32, a checksum-required command is refused (as the docs say)" "NotImplemented" \
    aws --profile record-store s3api put-bucket-versioning --bucket algorithm-probe --versioning-configuration Status=Enabled
fi

# Every documented checksum-required command passes --checksum-algorithm CRC32.
missing_flag=""
while IFS= read -r hit; do
  file="${hit%%:*}"; rest="${hit#*:}"; line="${rest%%:*}"
  # The flag may sit on any line of the same command (up to its closing line).
  if ! awk -v start="$line" 'NR>=start && NR<=start+4 { print } NR>start && !/\\$/ { exit }' "$repository_root/$file" \
      | grep -q -- "--checksum-algorithm CRC32"; then
    missing_flag+="$file:$line"$'\n'
  fi
done < <(cd "$repository_root" && grep -rnE "s3api (put-bucket-versioning|put-bucket-cors|put-object-retention|put-object-lock-configuration|put-object-legal-hold)" docs | grep -v '`s3api')
if [[ -z "$missing_flag" ]]; then
  pass "every documented checksum-required command passes --checksum-algorithm CRC32"
else
  fail "documented checksum-required commands without --checksum-algorithm CRC32" "$missing_flag"
fi

# ---------------------------------------------------------------------------
section "AWS CLI guide: common operations (docs/guides/aws-cli.md)"

rs() { aws --profile record-store "$@"; }
printf 'report body\n' > report.pdf

check "Buckets: create-bucket demo"         rs s3api create-bucket --bucket demo
check "Buckets: head-bucket demo"           rs s3api head-bucket --bucket demo
check "Buckets: list-buckets shows demo"    bash -c 'aws_bin="$1"; $aws_bin --profile record-store s3api list-buckets | grep -q "\"demo\""' _ "$aws_bin"
check "Objects: s3 cp upload"               rs s3 cp ./report.pdf s3://demo/reports/report.pdf
check "Objects: s3 cp download"             rs s3 cp s3://demo/reports/report.pdf ./downloaded.pdf
check "Objects: downloaded bytes match"     cmp report.pdf downloaded.pdf
check "Objects: s3 ls"                      rs s3 ls s3://demo/reports/
check "Versioning: put-bucket-versioning"   rs s3api put-bucket-versioning --checksum-algorithm CRC32 --bucket demo --versioning-configuration Status=Enabled
check "Versioning: upload report.pdf"       rs s3 cp ./report.pdf s3://demo/report.pdf
version_id="$(rs s3api list-object-versions --bucket demo --prefix report.pdf 2>/dev/null | json_field Versions.0.VersionId 2>/dev/null || true)"
if [[ -n "$version_id" ]]; then
  check "Versioning: get-object --version-id" rs s3api get-object --bucket demo --key report.pdf --version-id "$version_id" ./old.pdf
else
  fail "Versioning: list-object-versions returned a version ID"
fi
check "CORS: put-bucket-cors" rs s3api put-bucket-cors --checksum-algorithm CRC32 --bucket demo --cors-configuration '{
  "CORSRules": [{
    "AllowedOrigins": ["https://app.example.com"],
    "AllowedMethods": ["PUT", "GET", "HEAD"],
    "AllowedHeaders": ["content-type", "x-amz-*"],
    "ExposeHeaders": ["ETag", "x-amz-version-id"],
    "MaxAgeSeconds": 3600
  }]
}'
check "Copy: copy-object"                   rs s3api copy-object --bucket demo --key copy.pdf --copy-source demo/reports/report.pdf
presigned_url="$(rs s3 presign s3://demo/reports/report.pdf --expires-in 900 2>/dev/null || true)"
check "Presigned URLs: s3 presign output downloads with curl" \
  bash -c 'curl --fail --silent "$1" | cmp - "$2"' _ "$presigned_url" report.pdf
check "Objects: s3 rm"                      rs s3 rm s3://demo/reports/report.pdf

# ---------------------------------------------------------------------------
section "Credentials: --profile versus AWS_PROFILE (docs/guides/aws-cli.md, policies.md)"

account_json="$(rs_admin service-account create docs-check --endpoint "$api_endpoint" 2>&1)"
account_id="$(json_field account.id <<<"$account_json" 2>/dev/null)"
account_key="$(json_field credential.key_id <<<"$account_json" 2>/dev/null)"
account_secret="$(json_field secret_access_key <<<"$account_json" 2>/dev/null)"
if [[ -n "$account_id" && -n "$account_key" && -n "$account_secret" ]]; then
  pass "record-store service-account create returned an account ID and key pair"
else
  fail "record-store service-account create returned an account ID and key pair" "$account_json"
fi

# The account has no policy yet, so a request signed with its keys is refused.
check_fails "AWS_PROFILE + keys in the environment: the request uses the environment keys (403)" "AccessDenied" \
  env AWS_PROFILE=record-store AWS_ACCESS_KEY_ID="$account_key" AWS_SECRET_ACCESS_KEY="$account_secret" \
  $aws_bin s3 ls s3://demo/
check "--profile + keys in the environment: the request ignores the environment keys" \
  env AWS_ACCESS_KEY_ID="$account_key" AWS_SECRET_ACCESS_KEY="$account_secret" \
  $aws_bin --profile record-store s3 ls s3://demo/

# ---------------------------------------------------------------------------
section "Tutorial: file upload app (docs/guides/tutorial-upload-app.md)"

check "step 2: record-store bucket create uploads" rs_admin bucket create uploads --endpoint "$api_endpoint"
cat > upload-app-policy.json <<'JSON'
{
  "name": "upload-app",
  "description": "Read and write objects in the uploads bucket",
  "statements": [
    {
      "effect": "allow",
      "actions": ["s3:ListBucket"],
      "resources": ["bucket:uploads"]
    },
    {
      "effect": "allow",
      "actions": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
      "resources": ["bucket:uploads/*"]
    }
  ]
}
JSON
policy_json="$(rs_admin policy create ./upload-app-policy.json --endpoint "$api_endpoint" 2>&1)"
policy_id="$(json_field id <<<"$policy_json" 2>/dev/null)"
check "step 3: record-store policy attach" rs_admin policy attach "$policy_id" "$account_id" --endpoint "$api_endpoint"

cat > cors.json <<'JSON'
{
  "CORSRules": [
    {
      "AllowedOrigins": ["http://localhost:3000"],
      "AllowedMethods": ["GET", "PUT", "HEAD"],
      "AllowedHeaders": ["*"],
      "ExposeHeaders": ["ETag"],
      "MaxAgeSeconds": 3000
    }
  ]
}
JSON
check_fails "step 4 (old text): put-bucket-cors as the upload-app account is refused" "AccessDenied" \
  env AWS_PROFILE=record-store AWS_ACCESS_KEY_ID="$account_key" AWS_SECRET_ACCESS_KEY="$account_secret" \
  $aws_bin s3api put-bucket-cors --bucket uploads --cors-configuration file://cors.json
check "step 4 (new text): put-bucket-cors with the root profile" \
  rs s3api put-bucket-cors --bucket uploads --cors-configuration file://cors.json --checksum-algorithm CRC32
check "step 11: the upload-app account can write" \
  env AWS_PROFILE=record-store AWS_ACCESS_KEY_ID="$account_key" AWS_SECRET_ACCESS_KEY="$account_secret" \
  $aws_bin s3 cp ./report.pdf s3://uploads/report.pdf
check "step 11: AWS_PROFILE + upload-app keys: s3 ls --recursive" \
  env AWS_PROFILE=record-store AWS_ACCESS_KEY_ID="$account_key" AWS_SECRET_ACCESS_KEY="$account_secret" \
  $aws_bin s3 ls s3://uploads/ --recursive
check "policies.md: AWS_PROFILE + account keys: s3 ls s3://uploads/" \
  env AWS_PROFILE=record-store AWS_ACCESS_KEY_ID="$account_key" AWS_SECRET_ACCESS_KEY="$account_secret" \
  $aws_bin s3 ls s3://uploads/

# ---------------------------------------------------------------------------
section "Temporary credentials (docs/administration/temporary-credentials.md)"

temporary_json="$(rs_admin credential temporary "$account_id" --expires-in-seconds 3600 --endpoint "$api_endpoint" 2>&1)"
temporary_key="$(json_field credential.key_id <<<"$temporary_json" 2>/dev/null)"
temporary_secret="$(json_field secret_access_key <<<"$temporary_json" 2>/dev/null)"
if [[ -n "$temporary_key" && -n "$temporary_secret" ]]; then
  check "AWS_PROFILE + temporary keys: s3 ls s3://uploads/" \
    env AWS_PROFILE=record-store AWS_ACCESS_KEY_ID="$temporary_key" AWS_SECRET_ACCESS_KEY="$temporary_secret" \
    $aws_bin s3 ls s3://uploads/
else
  fail "record-store credential temporary returned a key pair" "$temporary_json"
fi

# ---------------------------------------------------------------------------
section "Multipart uploads (docs/guides/multipart-uploads.md)"

head -c 5242880 /dev/zero > part-1
printf 'last part\n' > part-2
upload_id="$(rs s3api create-multipart-upload --bucket demo --key big.bin 2>/dev/null | json_field UploadId 2>/dev/null)"
if [[ -n "$upload_id" ]]; then
  pass "create-multipart-upload returned an UploadId"
  etag_1="$(rs s3api upload-part --bucket demo --key big.bin --upload-id "$upload_id" --part-number 1 --body ./part-1 2>/dev/null | json_field ETag 2>/dev/null)"
  etag_2="$(rs s3api upload-part --bucket demo --key big.bin --upload-id "$upload_id" --part-number 2 --body ./part-2 2>/dev/null | json_field ETag 2>/dev/null)"
  [[ -n "$etag_1" && -n "$etag_2" ]] && pass "upload-part returned an ETag for each part" || fail "upload-part returned an ETag for each part"
  check "list-multipart-uploads" rs s3api list-multipart-uploads --bucket demo
  check "list-parts"             rs s3api list-parts --bucket demo --key big.bin --upload-id "$upload_id"
  check_fails "complete-multipart-upload with descending parts is InvalidPartOrder" "InvalidPartOrder" \
    rs s3api complete-multipart-upload --bucket demo --key big.bin --upload-id "$upload_id" \
    --multipart-upload "{\"Parts\":[{\"PartNumber\":2,\"ETag\":$(printf '%s' "$etag_2" | python3 -I -c 'import json,sys;print(json.dumps(sys.stdin.read()))')},{\"PartNumber\":1,\"ETag\":$(printf '%s' "$etag_1" | python3 -I -c 'import json,sys;print(json.dumps(sys.stdin.read()))')}]}"
  check "complete-multipart-upload" \
    rs s3api complete-multipart-upload --bucket demo --key big.bin --upload-id "$upload_id" \
    --multipart-upload "{\"Parts\":[{\"PartNumber\":1,\"ETag\":$(printf '%s' "$etag_1" | python3 -I -c 'import json,sys;print(json.dumps(sys.stdin.read()))')},{\"PartNumber\":2,\"ETag\":$(printf '%s' "$etag_2" | python3 -I -c 'import json,sys;print(json.dumps(sys.stdin.read()))')}]}"
else
  fail "create-multipart-upload returned an UploadId"
fi
abort_id="$(rs s3api create-multipart-upload --bucket demo --key abandoned.bin 2>/dev/null | json_field UploadId 2>/dev/null)"
check "abort-multipart-upload" rs s3api abort-multipart-upload --bucket demo --key abandoned.bin --upload-id "$abort_id"

# ---------------------------------------------------------------------------
section "Object Lock (docs/administration/object-lock.md)"

printf 'statement\n' > statement.pdf
check "create-bucket --object-lock-enabled-for-bucket" \
  rs s3api create-bucket --bucket records --object-lock-enabled-for-bucket
check "put-object with COMPLIANCE retention" \
  rs s3api put-object --bucket records --key statement.pdf --body ./statement.pdf \
  --object-lock-mode COMPLIANCE --object-lock-retain-until-date 2033-01-01T00:00:00Z
lock_version="$(rs s3api list-object-versions --bucket records --prefix statement.pdf 2>/dev/null | json_field Versions.0.VersionId 2>/dev/null)"
check "put-object-retention on a version" \
  rs s3api put-object-retention --checksum-algorithm CRC32 --bucket records --key statement.pdf --version-id "$lock_version" \
  --retention '{"Mode":"COMPLIANCE","RetainUntilDate":"2033-01-01T00:00:00Z"}'
check "put-object-lock-configuration" \
  rs s3api put-object-lock-configuration --checksum-algorithm CRC32 --bucket records --object-lock-configuration \
  '{"ObjectLockEnabled":"Enabled","Rule":{"DefaultRetention":{"Mode":"GOVERNANCE","Days":365}}}'
check "put-object-legal-hold" \
  rs s3api put-object-legal-hold --checksum-algorithm CRC32 --bucket records --key statement.pdf --version-id "$lock_version" \
  --legal-hold Status=ON

# ---------------------------------------------------------------------------
section "Concepts and security pages"

rs s3 cp ./report.pdf s3://demo/reports/2026/q1.pdf >/dev/null 2>&1
check "buckets-and-objects.md: list-objects-v2 --prefix --delimiter returns CommonPrefixes" \
  bash -c 'aws_bin="$1"; $aws_bin --profile record-store s3api list-objects-v2 --bucket demo --prefix reports/ --delimiter / | grep -q CommonPrefixes' _ "$aws_bin"
check "versioning.md: put-bucket-versioning" \
  rs s3api put-bucket-versioning --checksum-algorithm CRC32 --bucket demo --versioning-configuration Status=Enabled
check "encryption.md: s3 cp onto itself with --metadata-directive REPLACE" \
  rs s3 cp s3://demo/copy.pdf s3://demo/copy.pdf --metadata-directive REPLACE

# ---------------------------------------------------------------------------
section "Troubleshooting: --endpoint-url overrides the profile (troubleshooting/*.md)"

aws configure set endpoint_url http://127.0.0.1:9 --profile record-store
check_fails "with the profile endpoint pointing nowhere, the plain command fails" "Could not connect" \
  aws --profile record-store s3 ls
check "the bypass command reaches Record Store through --endpoint-url" \
  aws --profile record-store --endpoint-url "$s3_endpoint" s3 ls
check "the bypass upload command works" \
  aws --profile record-store --endpoint-url "$s3_endpoint" s3 cp ./report.pdf s3://uploads/large.bin
aws configure set endpoint_url "$s3_endpoint" --profile record-store

# ---------------------------------------------------------------------------
section "Deployment smoke tests (production-checklist.md, upgrading.md)"

echo "hello" > /tmp/smoke-$$.txt
check "s3 mb s3://smoke-test"            aws --profile record-store s3 mb s3://smoke-test
check "s3 cp /tmp/smoke.txt"             aws --profile record-store s3 cp /tmp/smoke-$$.txt s3://smoke-test/smoke.txt
check "s3 cp s3://smoke-test/smoke.txt -" aws --profile record-store s3 cp s3://smoke-test/smoke.txt -
check "s3 rm"                            aws --profile record-store s3 rm s3://smoke-test/smoke.txt
check "s3 rb"                            aws --profile record-store s3 rb s3://smoke-test
rm -f /tmp/smoke-$$.txt

# ---------------------------------------------------------------------------
section "Summary"

echo "  $passed passed, $failed failed"
for name in ${failures[@]+"${failures[@]}"}; do echo "  - $name"; done
cat <<'TEXT'

  Not covered by this script:
  - aws-chunked checksums: over plain HTTP, clients send checksums as headers,
    which Record Store accepts. Test the checksum settings through a TLS proxy.
  - path-style addressing: an IP endpoint always uses path-style. Test it
    through a hostname endpoint, such as storage.localhost behind a proxy.
  - AWS CLI 2.13, the documented minimum: run again with AWS_CLI set to it.
  - SDK examples: see tests/compatibility/run.sh.
TEXT

[[ $failed -eq 0 ]]
