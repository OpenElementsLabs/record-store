'use client';

import { useQuery } from '@tanstack/react-query';

import { ErrorState } from '@/components/error-state';
import { MetricCard, UsageBar } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge, StatusPending, type StatusLevel } from '@/components/status-badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import {
  deploymentModeLabel,
  useClusterEnabled,
  useDeployment,
} from '@/features/system/deployment';
import { queryKeys, useReadiness, useStorageStatus, useStorageUsage } from '@/hooks/use-system';
import { fetchClusterHealth } from '@/lib/api/cluster';
import { fetchClusterStatus } from '@/lib/api/cluster';
import {
  formatBytes,
  formatBytesOf,
  formatCount,
  formatDateTime,
  formatRelativeTime,
} from '@/lib/format';
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
        <CardContent className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
          <Detail label="Service readiness">
            <ReadinessBadge />
          </Detail>
          <Detail label="Mode">
            <span className="type-body">{deploymentModeLabel(info.mode)}</span>
          </Detail>
          <Detail label="Version">
            <span className="font-mono type-body">{info.version}</span>
          </Detail>
          <Detail label="Metadata">
            {usage.isPending ? (
              <StatusPending />
            ) : usage.isError ? (
              <StatusBadge level="unknown" label="Unavailable" />
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
              usage.isError ? (
                'Unavailable'
              ) : usage.data ? (
                formatCount(usage.data.object_count)
              ) : (
                <Skeleton className="h-7 w-16" />
              )
            }
            detail={
              usage.data
                ? formatBytesOf(usage.data.bytes_used, usage.data.physical_bytes)
                : undefined
            }
          />
        </div>
      )}

      {usage.isError ? (
        <Card>
          <ErrorState error={usage.error} onRetry={() => void usage.refetch()} />
        </Card>
      ) : null}
      <ReadinessExplanation />
      <p className="type-meta-subtle">
        {status.dataUpdatedAt
          ? `Storage last observed ${formatDateTime(new Date(status.dataUpdatedAt).toISOString())}. `
          : ''}
        Capacity observations do not establish object integrity.
      </p>

      {/* Subsystems renders in every mode: reporting what is not enabled is
          the point, not an omission. */}
      <Subsystems />

      {clusterEnabled ? <BackgroundWorkers /> : null}
    </>
  );
}

/**
 * Whether the server can actually serve, as opposed to merely answering.
 *
 * Liveness and readiness are different questions and an operator needs them
 * apart: a process that responds to every request while its storage is
 * read-only is live and not ready, and one generic "Responding" badge would
 * describe both as healthy.
 */
function ReadinessBadge() {
  const readiness = useReadiness();
  if (readiness.isPending) return <StatusPending />;
  const result = readiness.data;
  if (!result) return <StatusBadge level="unknown" label="Unknown" />;
  if (result.state === 'ready') return <StatusBadge level="healthy" label="Ready" />;
  if (result.state === 'not-ready') return <StatusBadge level="critical" label="Not ready" />;
  return <StatusBadge level="unknown" label="Unreachable" />;
}

/**
 * Says what readiness established, and what to do when it did not.
 *
 * A badge alone leaves an operator guessing which of two very different
 * incidents they have, so the failure states carry their reason and the
 * request identifier that correlates with the server's own log line.
 */
function ReadinessExplanation() {
  const readiness = useReadiness();
  const result = readiness.data;
  const observed = readiness.dataUpdatedAt
    ? formatDateTime(new Date(readiness.dataUpdatedAt).toISOString())
    : null;

  if (!result) return null;
  if (result.state === 'ready') {
    return (
      <p className="type-meta-subtle">
        {observed ? `Readiness last confirmed ${observed}. ` : ''}
        The server completed a write, synchronise, and delete probe against its storage path, so it
        can accept writes. This says nothing about the integrity of objects already stored.
      </p>
    );
  }
  if (result.state === 'not-ready') {
    return (
      <Card>
        <CardContent className="space-y-2">
          <p className="type-body text-danger" role="alert">
            The server is running but reports it cannot serve. It will refuse writes until this
            clears.
          </p>
          <p className="type-meta">{result.reason}</p>
          <p className="type-meta-subtle">
            Check the storage path for a full or read-only filesystem, then check the server log.
            {result.requestId ? ` Correlate with request ${result.requestId}.` : ''}
          </p>
        </CardContent>
      </Card>
    );
  }
  return (
    <Card>
      <CardContent className="space-y-2">
        <p className="type-body text-danger" role="alert">
          The management API could not be reached, so readiness is unknown.
        </p>
        <p className="type-meta">{result.reason}</p>
        <p className="type-meta-subtle">
          This is a connectivity or process problem rather than a storage one. Nothing on this page
          below is current.
        </p>
      </CardContent>
    </Card>
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
/**
 * Per-subsystem health, aware of what this deployment actually runs.
 *
 * A standalone server has no cluster, no replication and no repair. Reporting
 * those as failing would be wrong and would train operators to ignore the
 * screen, so they read as not enabled — a neutral state, distinct from broken.
 */
function Subsystems() {
  const clusterEnabled = useClusterEnabled();
  const status = useStorageStatus();
  const cluster = useQuery({
    queryKey: queryKeys.clusterHealth,
    queryFn: ({ signal }) => fetchClusterHealth(signal),
    enabled: clusterEnabled,
    refetchInterval: 30_000,
  });

  const reachable = status.isSuccess;
  const usage = useStorageUsage();
  const rows: readonly {
    readonly name: string;
    readonly detail: string;
    readonly state: StatusLevel;
    readonly label: string;
  }[] = [
    {
      name: 'Management API',
      detail: 'Serves this console and the CLI.',
      state: status.isPending ? 'unknown' : reachable ? 'healthy' : 'critical',
      label: status.isPending ? 'Checking' : reachable ? 'Responding' : 'Unavailable',
    },
    {
      name: 'Object storage',
      detail: 'Reads and writes object payloads.',
      state: status.isPending ? 'unknown' : reachable ? 'healthy' : 'critical',
      label: status.isPending ? 'Checking' : reachable ? 'Responding' : 'Unavailable',
    },
    {
      name: 'Metadata',
      detail: 'Holds buckets, objects, and versions.',
      state: usage.isPending ? 'unknown' : usage.isError ? 'critical' : 'healthy',
      label: usage.isPending ? 'Checking' : usage.isError ? 'Unavailable' : 'Responding',
    },
    {
      name: 'Consensus',
      detail: clusterEnabled
        ? 'Agrees metadata changes between members.'
        : 'Only used when running as a cluster.',
      state: clusterEnabled
        ? cluster.isError
          ? 'critical'
          : cluster.data
            ? cluster.data.metadata.status.writable
              ? 'healthy'
              : 'critical'
            : cluster.isError
              ? 'critical'
              : 'unknown'
        : 'disabled',
      label: clusterEnabled
        ? cluster.isError
          ? 'Unavailable'
          : cluster.data
            ? cluster.data.metadata.status.writable
              ? 'Writable'
              : 'No quorum'
            : cluster.isError
              ? 'Unavailable'
              : 'Checking'
        : 'Not enabled',
    },
    {
      name: 'Replication',
      detail: clusterEnabled
        ? 'Keeps the configured number of copies.'
        : 'A standalone server keeps a single copy.',
      state: clusterEnabled
        ? cluster.isError
          ? 'critical'
          : cluster.data
            ? cluster.data.data.health
            : 'unknown'
        : 'disabled',
      label: clusterEnabled
        ? cluster.isError
          ? 'Unavailable'
          : cluster.data
            ? capitalise(cluster.data.data.health)
            : cluster.isError
              ? 'Unavailable'
              : 'Checking'
        : 'Not enabled',
    },
  ];

  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Subsystems</CardTitle>
        <CardDescription>
          What this deployment runs, and whether each part is working. Components a standalone
          server does not use are reported as not enabled rather than as failures.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <ul className="divide-y divide-border">
          {rows.map((row) => (
            <li key={row.name} className="flex flex-wrap items-center gap-x-4 gap-y-1 py-2.5">
              <div className="min-w-0 flex-1">
                <p className="type-body">{row.name}</p>
                <p className="type-meta">{row.detail}</p>
              </div>
              {row.state === 'unknown' ? (
                <StatusPending />
              ) : (
                <StatusBadge level={row.state} label={row.label} />
              )}
            </li>
          ))}
        </ul>
      </CardContent>
    </Card>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}

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
                className="flex flex-wrap items-center justify-between gap-2 rounded-control border border-border px-3 py-2"
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
          <span className="type-meta-subtle">
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
