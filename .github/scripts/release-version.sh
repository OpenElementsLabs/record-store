#!/usr/bin/env bash
# Resolves the Record Store release version and refuses to continue when the
# Git tag and the versions recorded in the repository disagree.
#
# A release is only meaningful if `record-store --version` inside the published
# image reports the same number as the tag that produced it, so the mismatch is
# caught here rather than after a tag has already been pushed.
#
# Usage:
#   release-version.sh                 # print repository versions, no tag check
#   release-version.sh v0.1.1          # additionally require the tag to match
#
# Emits shell-style key=value lines on stdout, and appends the same lines to
# $GITHUB_OUTPUT when running inside GitHub Actions.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# The version under [workspace.package]; every crate inherits it with
# `version.workspace = true`.
cargo_version="$(
  awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/                     { in_section = 0 }
    in_section && /^version[[:space:]]*=/ {
      match($0, /"[^"]+"/)
      print substr($0, RSTART + 1, RLENGTH - 2)
      exit
    }
  ' "$root/Cargo.toml"
)"

# The first two-space-indented "version" key is the manifest's own, not a
# dependency's.
console_version="$(
  sed -n 's/^  "version": "\([^"]*\)".*/\1/p' "$root/console/package.json" | head -n 1
)"

if [[ -z "$cargo_version" ]]; then
  echo "could not read version from [workspace.package] in Cargo.toml" >&2
  exit 1
fi
if [[ -z "$console_version" ]]; then
  echo "could not read version from console/package.json" >&2
  exit 1
fi

if [[ "$cargo_version" != "$console_version" ]]; then
  echo "version mismatch: Cargo.toml is $cargo_version, console/package.json is $console_version" >&2
  exit 1
fi

version="$cargo_version"

if [[ $# -gt 0 && -n "${1:-}" ]]; then
  tag="$1"
  tag_version="${tag#v}"
  if [[ "$tag_version" != "$version" ]]; then
    echo "version mismatch: tag $tag implies $tag_version, repository is $version" >&2
    exit 1
  fi
fi

# SemVer, with optional pre-release and build metadata.
semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
if [[ ! "$version" =~ $semver ]]; then
  echo "version $version is not valid semantic versioning" >&2
  exit 1
fi

major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
prerelease="false"
if [[ -n "${BASH_REMATCH[4]:-}" ]]; then
  prerelease="true"
fi

# Pre-releases publish only the exact version and commit tags. Moving `latest`,
# `0.1`, or `0` onto a release candidate would hand it to everyone tracking a
# floating tag.
{
  echo "version=$version"
  echo "major=$major"
  echo "minor=$major.$minor"
  echo "prerelease=$prerelease"
} | tee -a "${GITHUB_OUTPUT:-/dev/null}"
