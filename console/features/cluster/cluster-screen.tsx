'use client';

import { useQuery } from '@tanstack/react-query';
import Link from 'next/link';

import { ErrorState } from '@/components/error-state';
import { MetricCard } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge, StatusPending } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { queryKeys } from '@/hooks/use-system';
import { fetchClusterStatus } from '@/lib/api/cluster';
import { formatBytes, formatCount, formatDateTime } from '@/lib/format';

/**
 * Cluster overview.
 *
 * Data availability and metadata availability are shown as separate dimensions,
 * because losing a quorum and losing replicas have different consequences and
 * different remedies.
 */
export function ClusterScreen() {
  const status = useQuery({
    queryKey: queryKeys.clusterStatus,
    queryFn: ({ signal }) => fetchClusterStatus(signal),
    refetchInterval: 15_000,
  });

  if (status.isError) {
    return (
      <>
        <PageHeader title="Cluster" />
        <Card>
          <ErrorState error={status.error} onRetry={() => void status.refetch()} />
        </Card>
      </>
    );
  }

  const data = status.data;

  return (
    <>
      <PageHeader
        title="Cluster"
        description="Membership, durability, and metadata consensus for this cluster."
        actions={
          <Button asChild variant="secondary">
            <Link href="/cluster/nodes">View nodes</Link>
          </Button>
        }
      />

      <Card>
        <CardContent className="flex flex-wrap items-center gap-6">
          <Dimension label="Cluster">
            {data ? (
              <StatusBadge level={data.health} label={capitalise(data.health)} />
            ) : (
              <StatusPending />
            )}
          </Dimension>
          <Dimension label="Object data">
            {data ? (
              <StatusBadge level={data.data.health} label={capitalise(data.data.health)} />
            ) : (
              <StatusPending />
            )}
          </Dimension>
          <Dimension label="Metadata quorum">
            {data ? (
              <StatusBadge
                level={data.metadata.status.health}
                label={`${data.metadata.status.healthy_members} of ${data.metadata.status.members} members`}
              />
            ) : (
              <StatusPending />
            )}
          </Dimension>
          <Dimension label="Writes">
            {data ? (
              <StatusBadge
                level={data.data.writable ? 'healthy' : 'critical'}
                label={data.data.writable ? 'Accepted' : 'Refused'}
              />
            ) : (
              <StatusPending />
            )}
          </Dimension>
        </CardContent>
      </Card>

      {data && (data.data.notes.length > 0 || data.metadata.status.notes.length > 0) ? (
        <Card>
          <CardHeader className="flex-col items-start">
            <CardTitle>What needs attention</CardTitle>
          </CardHeader>
          <CardContent>
            <ul className="space-y-1.5">
              {[...data.metadata.status.notes, ...data.data.notes].map((note) => (
                <li key={note} className="text-sm text-ink-muted">
                  {note}
                </li>
              ))}
            </ul>
          </CardContent>
        </Card>
      ) : null}

      <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
        <MetricCard
          label="Nodes"
          value={data ? formatCount(data.data.nodes) : <Skeleton className="h-7 w-12" />}
          detail={data ? `${data.data.healthy_nodes} healthy` : undefined}
        />
        <MetricCard
          label="Logical data"
          value={
            data ? formatBytes(data.replication.logical_bytes) : <Skeleton className="h-7 w-20" />
          }
          detail={
            data
              ? `${formatBytes(data.replication.physical_bytes)} physical at ${data.replication.replication_factor}×`
              : undefined
          }
        />
        <MetricCard
          label="Under-replicated"
          value={
            data ? (
              formatCount(data.replication.under_replicated_payloads)
            ) : (
              <Skeleton className="h-7 w-12" />
            )
          }
          detail={
            data && data.replication.unavailable_payloads > 0
              ? `${formatCount(data.replication.unavailable_payloads)} unreadable`
              : 'All payloads readable'
          }
        />
        <MetricCard
          label="Repair queue"
          value={data ? formatCount(data.repair.active_tasks) : <Skeleton className="h-7 w-12" />}
          detail={
            data && data.repair.parked_tasks > 0
              ? `${formatCount(data.repair.parked_tasks)} parked`
              : 'No parked work'
          }
        />
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader className="flex-col items-start">
            <CardTitle>Metadata consensus</CardTitle>
            <CardDescription>
              Cluster metadata is replicated by a consensus group formed from the storage nodes, so
              object requests keep working while the management plane restarts.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-2 text-sm">
            <Row label="Leader" value={data?.metadata.status.leader ?? '—'} />
            <Row label="This member's role" value={data?.metadata.role ?? '—'} />
            <Row
              label="Quorum required"
              value={
                data ? `${data.metadata.status.quorum} of ${data.metadata.status.members}` : '—'
              }
            />
            <Row
              label="Fault tolerant"
              value={
                data
                  ? data.metadata.status.fault_tolerant
                    ? 'Yes'
                    : 'No — at least three voting members are needed'
                  : '—'
              }
            />
            <Row label="Applied index" value={data?.metadata.applied_index?.toString() ?? '—'} />
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="flex-col items-start">
            <CardTitle>Active operations</CardTitle>
            <CardDescription>Drain, rebalance, and decommission work in progress.</CardDescription>
          </CardHeader>
          <CardContent>
            {data === undefined ? (
              <Skeleton className="h-16 w-full" />
            ) : data.operations.length === 0 ? (
              <p className="text-sm text-ink-muted">No cluster operations are running.</p>
            ) : (
              <ul className="space-y-2">
                {data.operations.map((operation) => (
                  <li
                    key={operation.id}
                    className="space-y-1 rounded-control border border-border px-3 py-2"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-sm font-medium text-ink">
                        {capitalise(operation.kind)}
                      </span>
                      <StatusBadge
                        level={operation.state === 'failed' ? 'critical' : 'pending'}
                        label={capitalise(operation.state)}
                      />
                    </div>
                    <p className="type-meta">
                      {formatCount(operation.progress.objects_remaining)} object(s) and{' '}
                      {formatBytes(operation.progress.bytes_remaining)} remaining ·{' '}
                      {operation.progress.replicas_moving} moving
                    </p>
                    {operation.message ? (
                      <p className="type-meta-subtle">{operation.message}</p>
                    ) : null}
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>
      </div>

      {data ? (
        <p className="type-meta-subtle">
          Observed {formatDateTime(data.observed_at)} · cluster {data.cluster_id}
        </p>
      ) : null}
    </>
  );
}

function Dimension({
  label,
  children,
}: {
  readonly label: string;
  readonly children: React.ReactNode;
}) {
  return (
    <div className="space-y-1">
      <p className="text-xs font-medium text-ink-muted">{label}</p>
      {children}
    </div>
  );
}

function Row({ label, value }: { readonly label: string; readonly value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="type-meta">{label}</span>
      <span className="type-body">{value}</span>
    </div>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
