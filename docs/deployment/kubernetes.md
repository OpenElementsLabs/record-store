# Kubernetes

Record Store publishes a Helm chart with every release. It runs one standalone
server as a StatefulSet, the console as a Deployment, and keeps the management
API off the network edge.

```bash
helm install record-store \
  oci://ghcr.io/openelementslabs/charts/record-store --version 0.2.0 \
  --namespace record-store --create-namespace \
  --set auth.rootAccessKey=admin \
  --set auth.rootSecretKey="$(openssl rand -hex 24)" \
  --set auth.credentialMasterKey="$(openssl rand -hex 32)" \
  --set auth.managementSystemToken="$(openssl rand -hex 32)"
```

The chart always runs exactly one server pod, and setting `replicaCount` fails the
install rather than being ignored. Read on before using it for anything you
intend to keep.

## What the chart creates

| Resource | Why |
| --- | --- |
| StatefulSet | One server pod on its own volume. Never runs two pods against that volume, even during an upgrade |
| Headless Service | Required by the StatefulSet; nothing connects to it |
| Service (S3) | Port 7600, the endpoint clients talk to |
| Service (management) | Port 7601, internal only |
| Deployment + Service (console) | Port 7602, stateless and interchangeable |
| Secret | Credentials, annotated `helm.sh/resource-policy: keep` |
| ConfigMap | `record-store.toml` |

The management API is never given a `LoadBalancer` or `NodePort` by this chart,
and CI asserts that it stays internal. It is unrestricted administrative
access. Reach it deliberately:

```bash
kubectl -n record-store port-forward svc/record-store-api 7601:7601
```

## Credentials

For a trial, passing credentials through `--set` is fine. For anything
long-lived it is not: Helm records values in the release history. Create the
Secret yourself instead.

```bash
kubectl -n record-store create secret generic record-store-auth \
  --from-literal=root-access-key=admin \
  --from-literal=root-secret-key="$(openssl rand -hex 24)" \
  --from-literal=credential-master-key="$(openssl rand -hex 32)" \
  --from-literal=management-system-token="$(openssl rand -hex 32)"
```

```yaml
auth:
  existingSecret: record-store-auth
```

The chart-managed Secret is kept when the release is uninstalled, so
reinstalling does not orphan credentials already encrypted with the master key.

!!! warning "Back up the credential master key somewhere other than Kubernetes"

    It cannot be rotated. Losing it makes every stored service-account
    credential unreadable, and with encryption enabled, every object.

## Availability

There is one server pod, so anything that stops it stops the service: an
upgrade, a configuration change, a node drain, a failed node. After an upgrade
or a drain, Kubernetes starts the pod again with the same volume, and clients
see errors until it passes its readiness probe.

A failed node is slower. Kubernetes does not start a replacement for a
StatefulSet pod while the old one might still be running, so the pod stays
unavailable until the node comes back or you delete the Node object. That wait
is what keeps two processes off one data directory.

Plan maintenance windows accordingly, and
protect the data the way [Durability](../concepts/durability.md) describes: a
redundant volume underneath and regular [backups](../operations/backup-and-restore.md).

## Storage

The volume is mounted at `/var/lib/record-store`, and the server keeps its data in
`data/` inside it, a directory it creates with its own permissions. Provisioners such
as local-path and many NFS ones create the volume root writable by everyone, which
the server refuses to store data in; the subdirectory is what makes those volumes
usable. Size memory with [Capacity Planning](../operations/capacity-planning.md#memory):
the chart requests 512 MiB and limits the pod to 2 GiB.

`persistence.enabled: false` puts objects in an `emptyDir` — useful for a trial,
never for anything else.

The data directory wants a local or network-attached block volume with real
`fsync` semantics. See [Persistent Storage](persistent-storage.md) for what the
storage layer expects. The chart cannot grow a StatefulSet's volumes for you: to
expand, your StorageClass must allow volume expansion, and you edit the PVCs
directly.

## Configuration

`values.yaml` carries `configuration` as raw TOML, rendered into the ConfigMap
unchanged. It is the same file documented in
[Configuration](../administration/configuration.md), so nothing about
configuring Record Store changes because it is running in Kubernetes.

```yaml
configuration: |
  [storage]
  data_directory = "/var/lib/record-store/data"
  encryption_enabled = true

  [observability]
  log_filter = "record_store=info"
  json = true
```

Listener addresses, the data directory and every credential are set by the
chart through the environment and override the file. Changing `configuration`
restarts the server, because the ConfigMap's checksum is part of the pod
template.

## Exposing it

The S3 endpoint is the only server port meant to be published.

```yaml
ingress:
  s3:
    enabled: true
    className: nginx
    host: storage.example.com
    annotations:
      nginx.ingress.kubernetes.io/proxy-body-size: "0"
    tls:
      - secretName: storage-tls
        hosts: [storage.example.com]
  console:
    enabled: true
    className: nginx
    host: record-store.example.com
    tls:
      - secretName: console-tls
        hosts: [record-store.example.com]
```

Objects are not small, and every ingress controller has a default body limit
lower than the uploads you intend to allow. Raise it explicitly — the annotation
above is the nginx spelling.

See [Reverse Proxy and TLS](reverse-proxy.md) for what belongs in front of which
port.

## Metrics

```yaml
auth:
  metricsScrapeToken: "..."   # or the metrics-scrape-token key in your Secret
metrics:
  podAnnotations:
    enabled: true
```

The endpoint stays closed while no token is set. See
[Metrics](../administration/metrics.md).

## Upgrading

```bash
helm upgrade record-store \
  oci://ghcr.io/openelementslabs/charts/record-store --version <new-version> \
  --namespace record-store --values my-values.yaml
```

Pass your values file again rather than `--reuse-values`, which keeps the previous
chart's values and drops any default the new chart adds. The StatefulSet stops the
old pod before it starts the new one, so an upgrade is a short outage rather than a
rolling one. Read [Upgrading](upgrading.md) for what
a version change can mean for stored data, and take a backup first.

## Air-gapped installs

The chart is attached to each release as a tarball, so it can be installed
without reaching a registry:

```bash
helm install record-store ./record-store-0.2.0.tgz --values my-values.yaml
```

You will also need to mirror `ghcr.io/openelementslabs/record-store` and
`…/record-store-console` into your own registry and point `image.repository` and
`console.image.repository` at it.
