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
import {
  deploymentModeLabel,
  useClusterEnabled,
  useDeployment,
} from '@/features/system/deployment';
import { AttentionPanel } from '@/features/overview/attention-panel';
import { RecentActivity } from '@/features/overview/recent-activity';
import { queryKeys, useStorageStatus, useStorageUsage } from '@/hooks/use-system';
import { fetchClusterHealth } from '@/lib/api/cluster';
import { fetchSystemMetrics } from '@/lib/api/system';
import { formatBytes, formatBytesOf, formatCount, formatRatio } from '@/lib/format';

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
            ? 'Health and capacity across this Record Store cluster.'
            : 'Health and capacity for this Record Store server.'
        }
      />

      <AttentionPanel />

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

      <TrafficPanel />

      <div className="grid gap-4 lg:grid-cols-3">
        <div className="lg:col-span-2">
          <RecentActivity />
        </div>
        <Card>
          <CardHeader>
            <CardTitle>Deployment</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3 text-sm">
            <Detail label="Mode" value={deploymentModeLabel(info.mode)} />
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

/**
 * Traffic counters since this server started.
 *
 * These are cumulative totals, and they are labelled as such. Record Store exposes
 * counters, not windowed rates, so presenting a "requests per second" here
 * would be a number the backend never measured.
 */
function TrafficPanel() {
  const metrics = useQuery({
    queryKey: queryKeys.systemMetrics,
    queryFn: ({ signal }) => fetchSystemMetrics(signal),
    refetchInterval: 30_000,
  });

  if (metrics.isError) {
    return (
      <Card>
        <ErrorState error={metrics.error} onRetry={() => void metrics.refetch()} />
      </Card>
    );
  }

  const data = metrics.data;
  const errorRatio = data && data.requests > 0 ? data.errors / data.requests : null;

  return (
    <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
      <MetricCard
        label="Requests served"
        value={data ? formatCount(data.requests) : <Skeleton className="h-7 w-20" />}
        detail="Since this server started"
      />
      <MetricCard
        label="Failed requests"
        value={data ? formatCount(data.errors) : <Skeleton className="h-7 w-16" />}
        detail={
          data && errorRatio !== null
            ? `${formatRatio(data.errors, data.requests)} of requests`
            : 'Since this server started'
        }
      />
      <MetricCard
        label="Uploaded"
        value={data ? formatBytes(data.upload_bytes) : <Skeleton className="h-7 w-24" />}
        detail="Bytes accepted from clients"
      />
      <MetricCard
        label="Downloaded"
        value={data ? formatBytes(data.download_bytes) : <Skeleton className="h-7 w-24" />}
        detail="Bytes served to clients"
      />
    </div>
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
      <span className="type-meta">{label}</span>
      <span className={mono ? 'break-all font-mono text-xs text-ink' : 'type-body'}>{value}</span>
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
      <CardContent className="flex flex-wrap items-center justify-between gap-3">
        <div className="space-y-1">
          <p className="text-xs font-medium text-ink-muted">Record Store status</p>
          {status.isPending ? (
            <StatusPending />
          ) : (
            <StatusBadge
              level={level === 'critical' ? 'critical' : 'healthy'}
              label={level === 'critical' ? 'Degraded' : 'Healthy'}
            />
          )}
        </div>
        <p className="max-w-md type-meta">
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
      <CardContent className="space-y-3">
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
              <li key={reason} className="type-meta">
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
