'use client';

import { useQuery } from '@tanstack/react-query';

import { ErrorState } from '@/components/error-state';
import { MetricCard, UsageBar } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge, StatusPending } from '@/components/status-badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useClusterEnabled, useDeployment } from '@/features/system/deployment';
import { queryKeys, useStorageStatus, useStorageUsage } from '@/hooks/use-system';
import { fetchClusterStatus } from '@/lib/api/cluster';
import { formatBytes, formatBytesOf, formatCount, formatRelativeTime } from '@/lib/format';
import type { BackgroundTaskStatus } from '@/types/cluster';

/**
 * System health.
 *
 * Only information the API deliberately exposes appears here. Filesystem paths,
 * key material, and internal addresses are intentionally absent.
 */
export function HealthScreen() {
  const { info } = useDeployment();
  const clusterEnabled = useClusterEnabled();
  const status = useStorageStatus();
  const usage = useStorageUsage();

  return (
    <>
      <PageHeader
        title="System health"
        description="Readiness, capacity, and background workers for this deployment."
      />

      <Card>
        <CardContent className="grid gap-4 pt-4 sm:grid-cols-2 lg:grid-cols-4">
          <Detail label="Status">
            {status.isPending ? (
              <StatusPending />
            ) : status.isError ? (
              <StatusBadge level="critical" label="Unavailable" />
            ) : (
              <StatusBadge level="healthy" label="Ready" />
            )}
          </Detail>
          <Detail label="Mode">
            <span className="text-sm text-ink">
              {info.mode === 'cluster'
                ? 'Cluster'
                : info.mode === 'control'
                  ? 'Control'
                  : 'Standalone'}
            </span>
          </Detail>
          <Detail label="Version">
            <span className="font-mono text-sm text-ink">{info.version}</span>
          </Detail>
          <Detail label="Metadata">
            {status.isError ? (
              <StatusBadge level="critical" label="Unreachable" />
            ) : (
              <StatusBadge level="healthy" label="Responding" />
            )}
          </Detail>
        </CardContent>
      </Card>

      {status.isError ? (
        <Card>
          <ErrorState error={status.error} onRetry={() => void status.refetch()} />
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <MetricCard
            label="Disk capacity"
            value={
              status.data ? (
                formatBytes(status.data.capacity_bytes)
              ) : (
                <Skeleton className="h-7 w-24" />
              )
            }
          />
          <MetricCard
            label="Available"
            value={
              status.data ? (
                formatBytes(status.data.available_bytes)
              ) : (
                <Skeleton className="h-7 w-24" />
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
          <MetricCard
            label="Incomplete uploads"
            value={
              status.data ? (
                formatBytes(status.data.temporary_upload_bytes)
              ) : (
                <Skeleton className="h-7 w-20" />
              )
            }
            detail="Staged bytes not yet committed"
          />
          <MetricCard
            label="Stored objects"
            value={
              usage.data ? formatCount(usage.data.object_count) : <Skeleton className="h-7 w-16" />
            }
            detail={
              usage.data
                ? formatBytesOf(usage.data.bytes_used, usage.data.physical_bytes)
                : undefined
            }
          />
        </div>
      )}

      {clusterEnabled ? <BackgroundWorkers /> : null}
    </>
  );
}

function Detail({
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

/**
 * Liveness of this node's supervised background services.
 *
 * A service that stopped unexpectedly is reported rather than hidden, because
 * repair and rebalancing failing silently is worse than a visible warning.
 */
function BackgroundWorkers() {
  const status = useQuery({
    queryKey: queryKeys.clusterStatus,
    queryFn: ({ signal }) => fetchClusterStatus(signal),
    refetchInterval: 20_000,
  });

  if (status.isError) {
    return (
      <Card>
        <ErrorState error={status.error} onRetry={() => void status.refetch()} />
      </Card>
    );
  }

  const tasks = Object.entries(status.data?.local_tasks ?? {});

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Background workers</CardTitle>
        <CardDescription>
          Supervised services on this node. A stopped worker degrades readiness.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {status.isPending ? (
          <Skeleton className="h-16 w-full" />
        ) : tasks.length === 0 ? (
          <p className="text-sm text-ink-muted">No background workers are registered.</p>
        ) : (
          <ul className="space-y-2">
            {tasks.map(([name, task]) => (
              <li
                key={name}
                className="flex flex-wrap items-center justify-between gap-2 rounded-[--radius-control] border border-border px-3 py-2"
              >
                <span className="font-mono text-xs text-ink">{name}</span>
                <WorkerStatus task={task} />
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

function WorkerStatus({ task }: { readonly task: BackgroundTaskStatus }) {
  if (task.state === 'running') {
    return (
      <span className="flex items-center gap-2">
        <StatusBadge level="healthy" label="Running" />
        {task.last_pass_at ? (
          <span className="text-xs text-ink-subtle">
            last pass {formatRelativeTime(task.last_pass_at)}
          </span>
        ) : null}
      </span>
    );
  }
  if (task.state === 'stopped') return <StatusBadge level="paused" label="Stopped" />;
  return (
    <span className="flex flex-wrap items-center gap-2">
      <StatusBadge level="critical" label="Failed" />
      <span className="text-xs text-danger">{task.reason}</span>
    </span>
  );
}
