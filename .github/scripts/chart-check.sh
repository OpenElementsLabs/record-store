#!/usr/bin/env bash
# Lints the Helm chart and renders it in the shapes that behave differently, so
# a template that only breaks with a NetworkPolicy, or only when an Ingress is
# asked for, fails here rather than in somebody's cluster.
#
# Renders are validated against the real Kubernetes schemas with kubeconform,
# which runs from its container so nothing has to be installed.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
chart="${root}/deploy/kubernetes/helm/record-store"
output="$(mktemp -d)"
trap 'rm -rf "$output"' EXIT

kubeconform="${KUBECONFORM_IMAGE:-ghcr.io/yannh/kubeconform:v0.7.0}"
kubernetes_version="${KUBERNETES_VERSION:-1.29.0}"

# Placeholder credentials that satisfy the chart's length rules. These exist
# only to make the templates render; nothing here is ever deployed.
secret="$(printf 'a%.0s' {1..64})"
token="$(printf 'b%.0s' {1..64})"
common=(
    --set "auth.rootAccessKey=render-check"
    --set "auth.rootSecretKey=${secret}"
    --set "auth.credentialMasterKey=${secret}"
    --set "auth.managementSystemToken=${token}"
)

echo "==> helm lint"
helm lint "$chart" "${common[@]}"

render() {
    local name="$1"
    shift
    echo "==> render: ${name}"
    helm template render-check "$chart" "${common[@]}" "$@" > "${output}/${name}.yaml"
}

render standalone
render standalone-no-console --set console.enabled=false
render standalone-ephemeral --set persistence.enabled=false
render network-policy --set networkPolicy.enabled=true
render ingress \
    --set ingress.s3.enabled=true \
    --set ingress.console.enabled=true \
    --set ingress.s3.className=nginx \
    --set ingress.console.className=nginx

echo "==> kubeconform (Kubernetes ${kubernetes_version})"
docker run --rm --volume "${output}:/manifests:ro" "$kubeconform" \
    -strict -summary -kubernetes-version "$kubernetes_version" \
    /manifests

# Cheap assertions about things that are easy to break and expensive to notice.
echo "==> invariants"
# The chart runs one standalone server. Nothing may configure more pods or any
# multi-node setting, and asking for more replicas must fail rather than be
# silently ignored.
for manifest in "${output}"/*.yaml; do
    if grep -qE 'RECORD_STORE_CLUSTER_|RECORD_STORE_RPC_|value: cluster$' "$manifest"; then
        echo "$(basename "$manifest"): renders multi-node configuration" >&2
        exit 1
    fi
    if ! awk '/^kind: StatefulSet/,/^---/' "$manifest" | grep -q '^  replicas: 1$'; then
        echo "$(basename "$manifest"): the StatefulSet must run exactly one replica" >&2
        exit 1
    fi
done
if helm template render-check "$chart" "${common[@]}" --set replicaCount=3 > /dev/null 2>&1; then
    echo "setting replicaCount must fail the render" >&2
    exit 1
fi
# The management API is the control plane. No render may publish it outside the
# cluster; the S3 service, by contrast, is allowed to be a LoadBalancer.
for manifest in "${output}"/*.yaml; do
    if grep -q 'record-store-api' "$manifest" \
        && awk '/name: render-check-record-store-api/,/^---/' "$manifest" \
        | grep -qE 'type: (LoadBalancer|NodePort)'; then
        echo "$(basename "$manifest"): the management API service must stay ClusterIP" >&2
        exit 1
    fi
done

echo "chart checks passed"
