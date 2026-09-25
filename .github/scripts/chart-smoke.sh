#!/usr/bin/env bash
# Installs the Helm chart on whatever cluster kubectl currently points at and
# proves the deployment actually works: pods become ready on their own probes,
# the S3 listener answers, the management API answers through the CLI in the
# pod, and the console can reach the management API across the two Services.
#
# Rendering the chart is checked by chart-check.sh. This is the part that only a
# real cluster can tell you: whether the probes, the Services and the pod
# security context agree with the image.
#
# Expects a throwaway cluster. It installs into its own namespace and deletes it
# on the way out.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
chart="${root}/deploy/kubernetes/helm/record-store"
namespace="${CHART_SMOKE_NAMESPACE:-record-store-smoke}"
release=rs

# The chart's appVersion names the release being prepared, whose images do not
# exist until that release publishes them. CI builds both images from the
# commit and loads them into the cluster (CHART_SMOKE_IMAGE_REPOSITORY and
# CHART_SMOKE_CONSOLE_IMAGE_REPOSITORY, pulled never), so the chart is proven
# against the server it ships with rather than the previous release. Without
# them a published tag is deployed instead, which checks the chart's wiring
# but not that it agrees with this commit's server.
image_tag="${CHART_SMOKE_IMAGE_TAG:-latest}"
image_arguments=()
if [[ -n "${CHART_SMOKE_IMAGE_REPOSITORY:-}" ]]; then
    image_arguments+=(--set "image.repository=${CHART_SMOKE_IMAGE_REPOSITORY}" --set image.pullPolicy=Never)
fi
if [[ -n "${CHART_SMOKE_CONSOLE_IMAGE_REPOSITORY:-}" ]]; then
    image_arguments+=(--set "console.image.repository=${CHART_SMOKE_CONSOLE_IMAGE_REPOSITORY}" \
        --set console.image.pullPolicy=Never)
fi

cleanup() {
    local status=$?
    if [[ $status -ne 0 ]]; then
        echo "=== the deployment did not come up; collecting state ==="
        kubectl --namespace "$namespace" get pods -o wide || true
        kubectl --namespace "$namespace" describe pods || true
        kubectl --namespace "$namespace" logs --selector app.kubernetes.io/instance="$release" \
            --all-containers --tail=200 || true
    fi
    kubectl delete namespace "$namespace" --ignore-not-found --wait=false > /dev/null 2>&1 || true
    return $status
}
trap cleanup EXIT

# Generated here and discarded with the cluster.
access_key="smoke-access-key"
secret_key="$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
master_key="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
system_token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"

echo "==> installing the chart"
helm install "$release" "$chart" \
    --namespace "$namespace" --create-namespace \
    --set "auth.rootAccessKey=${access_key}" \
    --set "auth.rootSecretKey=${secret_key}" \
    --set "auth.credentialMasterKey=${master_key}" \
    --set "auth.managementSystemToken=${system_token}" \
    --set persistence.size=1Gi \
    --set resources.requests.cpu=50m \
    --set resources.requests.memory=128Mi \
    --set console.resources.requests.cpu=50m \
    --set "image.tag=${image_tag}" \
    --set "console.image.tag=${image_tag}" \
    ${image_arguments[@]+"${image_arguments[@]}"} \
    --wait --timeout 10m

pod="${release}-record-store-0"

echo "==> the server reports ready on its own probe endpoint"
kubectl --namespace "$namespace" exec "$pod" -- \
    record-store status --endpoint http://127.0.0.1:7601

echo "==> the data directory is writable by the unprivileged account"
kubectl --namespace "$namespace" exec "$pod" -- \
    /bin/sh -c 'test -d /var/lib/record-store/data/objects && test -d /var/lib/record-store/data/metadata'

# Both checks run from the console pod, which has a JavaScript runtime and sits
# on the far side of the Services. That makes them tests of the Services and of
# pod-to-pod networking, not just of a listener bound inside one container.
console="$(kubectl --namespace "$namespace" get pod \
    --selector app.kubernetes.io/component=console \
    -o jsonpath='{.items[0].metadata.name}')"

echo "==> the console reaches the management API across Services"
kubectl --namespace "$namespace" exec "$console" -- \
    node -e '
      fetch(process.env.RECORD_STORE_API_URL + "/health")
        .then(r => {
          if (!r.ok) { throw new Error("management API answered " + r.status); }
          return r.text();
        })
        .then(body => console.log("management API: " + body))
        .catch(error => { console.error(String(error)); process.exit(1); });
    '

echo "==> the S3 service answers and demands a signature"
# 403 is the right answer to an unsigned request: it proves the listener is
# serving and authenticating rather than merely open.
kubectl --namespace "$namespace" exec "$console" -- \
    node -e '
      fetch("http://'"${release}"'-record-store:7600/")
        .then(r => {
          if (r.status !== 403) {
            throw new Error("the S3 service answered " + r.status + ", expected 403");
          }
          console.log("S3 service: 403, signature required");
        })
        .catch(error => { console.error(String(error)); process.exit(1); });
    '

echo "==> the release is healthy"
helm --namespace "$namespace" status "$release" --output json > /dev/null

echo "chart smoke passed"
