#!/usr/bin/env bash
# Refuses to let a release publish without its signed provenance and SBOMs.
#
# Every attest step in this pipeline is a step that can be removed, renamed, or
# quietly skipped by a conditional that stopped matching. None of those failures
# announce themselves: the release still builds, the assets still upload, and the
# only symptom is that `gh attestation verify` starts failing for whoever
# downloads it — months later, with no way to tell whether the build was
# compromised or the workflow was simply wrong.
#
# So the claim is checked rather than assumed, against the same public
# attestation service a user queries, and the release is refused if anything is
# missing. This is the difference between a pipeline that produces provenance and
# one that is documented as producing provenance.
#
# Usage:
#   verify-attestations.sh REPOSITORY ASSET_DIR \
#       SERVER_IMAGE SERVER_DIGEST CONSOLE_IMAGE CONSOLE_DIGEST
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 REPOSITORY ASSET_DIR SERVER_IMAGE SERVER_DIGEST CONSOLE_IMAGE CONSOLE_DIGEST" >&2
  exit 2
fi

repository="$1"
asset_directory="$2"
server_image="$3"
server_digest="$4"
console_image="$5"
console_digest="$6"

script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Attestations are published moments before this runs, and the service is not
# instantaneous. A single failed lookup here would make releases flaky, which is
# the fastest way to get a safety check disabled — so a miss is retried before it
# is believed.
attempts="${ATTESTATION_VERIFY_ATTEMPTS:-5}"
delay="${ATTESTATION_VERIFY_DELAY:-10}"

missing=()

# Runs `gh attestation verify`, retrying a miss before treating it as absent.
verify() {
  local description="$1"
  shift
  local attempt=1
  local output
  while true; do
    if output="$(gh attestation verify "$@" --repo "$repository" 2>&1)"; then
      echo "  ok        ${description}"
      return 0
    fi
    if [[ $attempt -ge $attempts ]]; then
      echo "  MISSING   ${description}"
      while IFS= read -r line; do
        echo "              ${line}" >&2
      done <<< "${output}"
      missing+=("$description")
      return 0
    fi
    echo "  retrying  ${description} (attempt ${attempt} of ${attempts})"
    attempt=$((attempt + 1))
    sleep "$delay"
  done
}

echo "Verifying release attestations against ${repository}"

echo
echo "Container image provenance"
verify "provenance: ${server_image}@${server_digest}" \
  "oci://${server_image}@${server_digest}"
verify "provenance: ${console_image}@${console_digest}" \
  "oci://${console_image}@${console_digest}"

# An SBOM describes one architecture, and is attested against the platform
# manifest rather than the index. Verifying the index would pass while an
# architecture's SBOM was missing entirely.
echo
echo "Per-architecture SBOM attestations"
for entry in "${server_image}|${server_digest}" "${console_image}|${console_digest}"; do
  image="${entry%%|*}"
  index="${entry##*|}"
  for architecture in amd64 arm64; do
    if ! platform_digest="$("${script_directory}/platform-digest.sh" "$image" "$index" "$architecture")"; then
      echo "  MISSING   SBOM: ${image} ${architecture} (no platform manifest)"
      missing+=("SBOM: ${image} ${architecture} (no platform manifest)")
      continue
    fi
    verify "SBOM: ${image} ${architecture}" \
      "oci://${image}@${platform_digest}" \
      --predicate-type https://spdx.dev/Document
  done
done

# The binary archives are downloaded and run directly, without a registry in
# between, so they need provenance at least as much as the images do.
echo
echo "Binary archive provenance"
shopt -s nullglob
archives=("${asset_directory}"/*.tar.gz)
if [[ ${#archives[@]} -eq 0 ]]; then
  echo "  MISSING   no binary archives were produced"
  missing+=("binary archives")
fi
for archive in "${archives[@]}"; do
  verify "provenance: $(basename "$archive")" "$archive"
done

# The checks above ask GitHub's attestation service. That service is not what a
# downloader ends up holding: the release page is. So the bundle attached to the
# release is checked as its own artefact, because "the API has provenance" and
# "the release ships provenance" are different claims and only the second one
# survives someone archiving the download.
#
# This is a structural check, not a second signature check — it confirms the
# bundle exists, decodes, and names every archive by digest. A bundle copied
# from the wrong run, or one that silently lost a subject when an architecture
# was added, fails here.
echo
echo "Published provenance bundle"
bundles=("${asset_directory}"/*.intoto.jsonl)
if [[ ${#bundles[@]} -eq 0 ]]; then
  echo "  MISSING   no provenance bundle is attached to the release"
  missing+=("provenance bundle asset")
elif [[ ${#bundles[@]} -gt 1 ]]; then
  echo "  MISSING   ${#bundles[@]} provenance bundles found, expected exactly one"
  missing+=("provenance bundle asset (ambiguous)")
else
  bundle="${bundles[0]}"
  echo "  ok        bundle: $(basename "$bundle")"
  # Each line is a Sigstore bundle whose DSSE payload is the in-toto statement.
  attested="$(
    jq -r '
      select(.dsseEnvelope.payload != null)
      | .dsseEnvelope.payload
      | @base64d
      | fromjson
      | .subject[]?
      | .digest.sha256 // empty
    ' "$bundle" 2>/dev/null | sort -u || true
  )"
  if [[ -z "$attested" ]]; then
    echo "  MISSING   the bundle names no subjects"
    missing+=("provenance bundle subjects")
  fi
  for archive in "${archives[@]}"; do
    digest="$(sha256sum "$archive" | cut -d' ' -f1)"
    if grep -qxF -- "$digest" <<< "$attested"; then
      echo "  ok        bundle covers $(basename "$archive")"
    else
      echo "  MISSING   the bundle does not cover $(basename "$archive") (${digest})"
      missing+=("bundle coverage: $(basename "$archive")")
    fi
  done
fi

echo
if [[ ${#missing[@]} -gt 0 ]]; then
  echo "Refusing to publish: ${#missing[@]} attestation(s) missing." >&2
  for item in "${missing[@]}"; do
    echo "  - ${item}" >&2
  done
  echo >&2
  echo "A release without provenance cannot be verified by anyone who downloads it." >&2
  echo "Fix the attest steps in the workflow rather than skipping this check." >&2
  exit 1
fi

echo "All attestations present."
