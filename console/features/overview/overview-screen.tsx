'use client';

import { useQuery } from '@tanstack/react-query';
import Link from 'next/link';

import { ErrorState } from '@/components/error-state';
import { MetricCard, UsageBar } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge, StatusPending } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useClusterEnabled, useDeployment } from '@/features/system/deployment';
import { RecentActivity } from '@/features/overview/recent-activity';
import { queryKeys, useStorageStatus, useStorageUsage } from '@/hooks/use-system';
import { fetchClusterHealth } from '@/lib/api/cluster';
import { formatBytes, formatBytesOf, formatCount } from '@/lib/format';

/**
 * The operational landing screen.
 *
 * Every figure here answers a question an operator actually asks: is it healthy,
 * how much room is left, how much is stored, and what changed recently.
 */
export function OverviewScreen() {
  const { info } = useDeployment();
  const clusterEnabled = useClusterEnabled();
  const usage = useStorageUsage();
  const status = useStorageStatus();

  return (
    <>
      <PageHeader
        title="Overview"
        description={
          clusterEnabled
            ? 'Health and capacity across this OES cluster.'
            : 'Health and capacity for this OES server.'
        }
      />

      {clusterEnabled ? <ClusterHealthPanel /> : <StandaloneHealthPanel />}

      {usage.isError ? (
        <Card>
          <ErrorState error={usage.error} onRetry={() => void usage.refetch()} />
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <MetricCard
            label="Stored data"
            value={
              usage.data ? formatBytes(usage.data.bytes_used) : <Skeleton className="h-7 w-24" />
            }
            detail={
              usage.data
                ? `${formatCount(usage.data.version_count)} version(s) retained`
                : undefined
            }
          />
          <MetricCard
            label="Objects"
            value={
              usage.data ? formatCount(usage.data.object_count) : <Skeleton className="h-7 w-16" />
            }
            detail={usage.data ? `${formatCount(usage.data.bucket_count)} bucket(s)` : undefined}
          />
          <MetricCard
            label="Physical usage"
            value={
              usage.data ? (
                formatBytes(usage.data.physical_bytes)
              ) : (
                <Skeleton className="h-7 w-24" />
              )
            }
            detail={
              usage.data
                ? `${formatBytes(usage.data.temporary_multipart_bytes)} in multipart parts`
                : undefined
            }
          />
          <MetricCard
            label="Disk capacity"
            value={
              status.data ? (
                formatBytesOf(
                  status.data.capacity_bytes - status.data.available_bytes,
                  status.data.capacity_bytes,
                )
              ) : (
                <Skeleton className="h-7 w-32" />
              )
            }
            footer={
              status.data ? (
                <UsageBar
                  used={status.data.capacity_bytes - status.data.available_bytes}
                  total={status.data.capacity_bytes}
                  label="Disk utilisation"
                />
              ) : undefined
            }
          />
        </div>
      )}

      <div className="grid gap-4 lg:grid-cols-3">
        <div className="lg:col-span-2">
          <RecentActivity />
        </div>
        <Card>
          <CardHeader>
            <CardTitle>Deployment</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3 text-sm">
            <Detail label="Mode" value={info.mode === 'cluster' ? 'Cluster' : 'Standalone'} />
            <Detail label="Version" value={info.version} />
            {info.cluster_id ? <Detail label="Cluster ID" value={info.cluster_id} mono /> : null}
            <div className="pt-1">
              <Button asChild size="sm" variant="secondary">
                <Link href="/system">View system health</Link>
              </Button>
            </div>
          </CardContent>
        </Card>
      </div>
    </>
  );
}

function Detail({
  label,
  value,
  mono = false,
}: {
  readonly label: string;
  readonly value: string;
  readonly mono?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-xs text-ink-muted">{label}</span>
      <span className={mono ? 'break-all font-mono text-xs text-ink' : 'text-sm text-ink'}>
        {value}
      </span>
    </div>
  );
}

/**
 * Standalone health.
 *
 * A single-server deployment is reported as one healthy server, not as a
 * one-node cluster with missing redundancy.
 */
function StandaloneHealthPanel() {
  const status = useStorageStatus();
  const level = status.isError ? 'critical' : status.isPending ? 'unknown' : 'healthy';
  return (
    <Card>
      <CardContent className="flex flex-wrap items-center justify-between gap-3 pt-4">
        <div className="space-y-1">
          <p className="text-xs font-medium text-ink-muted">OES status</p>
          {status.isPending ? (
            <StatusPending />
          ) : (
            <StatusBadge
              level={level === 'critical' ? 'critical' : 'healthy'}
              label={level === 'critical' ? 'Degraded' : 'Healthy'}
            />
          )}
        </div>
        <p className="max-w-md text-xs text-ink-muted">
          {status.isError
            ? 'Storage status could not be read from the management API.'
            : 'Storage and metadata are responding to readiness probes.'}
        </p>
      </CardContent>
    </Card>
  );
}

/** Cluster health, showing data and metadata as separate dimensions. */
function ClusterHealthPanel() {
  const health = useQuery({
    queryKey: queryKeys.clusterHealth,
    queryFn: ({ signal }) => fetchClusterHealth(signal),
    refetchInterval: 15_000,
  });

  if (health.isError) {
    return (
      <Card>
        <ErrorState error={health.error} onRetry={() => void health.refetch()} />
      </Card>
    );
  }

  return (
    <Card>
      <CardContent className="space-y-3 pt-4">
        <div className="flex flex-wrap items-center gap-6">
          <div className="space-y-1">
            <p className="text-xs font-medium text-ink-muted">Cluster status</p>
            {health.isPending ? (
              <StatusPending />
            ) : (
              <StatusBadge level={health.data.health} label={capitalise(health.data.health)} />
            )}
          </div>
          <div className="space-y-1">
            <p className="text-xs font-medium text-ink-muted">Object data</p>
            {health.isPending ? (
              <StatusPending />
            ) : (
              <StatusBadge
                level={health.data.data.health}
                label={capitalise(health.data.data.health)}
              />
            )}
          </div>
          <div className="space-y-1">
            <p className="text-xs font-medium text-ink-muted">Metadata quorum</p>
            {health.isPending ? (
              <StatusPending />
            ) : (
              <StatusBadge
                level={health.data.metadata.status.health}
                label={`${health.data.metadata.status.healthy_members} of ${health.data.metadata.status.members} members`}
              />
            )}
          </div>
        </div>
        {health.data && health.data.reasons.length > 0 ? (
          <ul className="space-y-1 border-t border-border pt-3">
            {health.data.reasons.map((reason) => (
              <li key={reason} className="text-xs text-ink-muted">
                {reason}
              </li>
            ))}
          </ul>
        ) : null}
      </CardContent>
    </Card>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
