#!/usr/bin/env bash
# Builds the Record Store .deb and .rpm for one architecture from binaries that
# already exist.
#
#   deploy/linux/build-packages.sh <arch> <version> <binary-directory> [output]
#
#   arch              amd64 or arm64, as nfpm names them
#   version           release version without a leading v, e.g. 0.1.3
#   binary-directory  holds record-store and record-store-server
#   output            where the packages land; defaults to dist
#
# The binaries are expected to be the statically linked musl builds. Both
# packages declare no libc dependency, so shipping dynamically linked binaries
# here would produce a package that installs cleanly and then fails to run.
#
# nfpm runs from its official container so that neither CI nor a developer needs
# it installed. Set NFPM_IMAGE to override.
set -euo pipefail

if [[ $# -lt 3 ]]; then
    sed -n '2,15p' "$0" >&2
    exit 2
fi

architecture="$1"
version="$2"
binaries="$(cd "$3" && pwd)"
output="${4:-dist}"
repository="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
image="${NFPM_IMAGE:-goreleaser/nfpm:v2.44.0}"

case "$architecture" in
    amd64 | arm64) ;;
    *)
        echo "unsupported architecture '${architecture}': expected amd64 or arm64" >&2
        exit 1
        ;;
esac

for binary in record-store record-store-server; do
    if [[ ! -f "${binaries}/${binary}" ]]; then
        echo "missing ${binary} in ${binaries}" >&2
        exit 1
    fi
done

# A dynamically linked binary here would produce a package that installs on a
# system it cannot run on, and the failure would surface as a crash at first
# start rather than as a dependency error. Catch it while building instead.
if command -v file > /dev/null 2>&1; then
    for binary in record-store record-store-server; do
        description="$(file -b "${binaries}/${binary}")"
        if [[ "$description" != *"statically linked"* ]]; then
            echo "${binary} is not statically linked: ${description}" >&2
            echo "build it for a *-unknown-linux-musl target before packaging" >&2
            exit 1
        fi
    done
fi

staging="${repository}/dist/staging"
mkdir -p "$staging" "${repository}/${output}"
install -m 0755 "${binaries}/record-store" "${staging}/record-store"
install -m 0755 "${binaries}/record-store-server" "${staging}/record-store-server"

for packager in deb rpm; do
    echo "building ${packager} for ${architecture} ${version}"
    docker run --rm \
        --volume "${repository}:/work" \
        --workdir /work \
        --env "PKG_ARCH=${architecture}" \
        --env "PKG_VERSION=${version}" \
        "$image" package \
        --config deploy/linux/nfpm.yaml \
        --packager "$packager" \
        --target "$output"
done

rm -rf "$staging"
ls -l "${repository}/${output}"
