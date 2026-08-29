#!/usr/bin/env bash
# Prints the digest of one platform's manifest within a multi-platform image
# index.
#
# Addressing a platform by its own digest, rather than by the index digest plus
# `--platform`, is what lets a single job handle both architectures: Docker's
# image store maps one digest reference to one image, so pulling a second
# platform under the index digest fails with `cannot overwrite digest`.
#
# Usage:
#   platform-digest.sh <image> <index-digest> <architecture>
set -euo pipefail

image="${1:?image}"
index="${2:?index digest}"
architecture="${3:?architecture}"

digest="$(
  docker buildx imagetools inspect "${image}@${index}" --format '{{json .Manifest}}' \
    | jq -r --arg architecture "$architecture" '
        .manifests[]
        | select(.platform.os == "linux" and .platform.architecture == $architecture)
        | .digest
      ' \
    | head -n 1
)"

if [[ "$digest" != sha256:* ]]; then
  echo "${index} has no linux/${architecture} manifest" >&2
  exit 1
fi

echo "$digest"
