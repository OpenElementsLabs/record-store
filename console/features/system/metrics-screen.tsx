'use client';

import { RefreshCw } from 'lucide-react';

import { ErrorState } from '@/components/error-state';
import { MetricCard, UsageBar } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { RateChart } from '@/features/system/rate-chart';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import {
  SAMPLE_INTERVAL_MS,
  useMetricsSamples,
  type Rate,
} from '@/features/system/use-metrics-samples';
import { formatBytes, formatCount, formatDuration, formatRatio } from '@/lib/format';

/**
 * Operational metrics.
 *
 * Record Store exposes counters. Rates are derived here by comparing readings, which is
 * the only way a rate can exist, and the screen says how long it has been
 * watching so nobody mistakes a short window for a server-side average.
 */
export function MetricsScreen() {
  const observation = useMetricsSamples();
  const { current } = observation;

  return (
    <>
      <PageHeader
        title="Metrics"
        description={`Counters are read every ${Math.round(SAMPLE_INTERVAL_MS / 1000)} seconds. Rates are measured across the window this page has been open.`}
        actions={
          <Button
            size="sm"
            // The name stays stable while the visible text reports progress, so
            // the control does not rename itself mid-interaction.
            aria-label="Refresh metrics"
            disabled={observation.isFetching}
            onClick={observation.refetch}
          >
            <RefreshCw aria-hidden className={observation.isFetching ? 'animate-spin' : ''} />
            <span aria-hidden>{observation.isFetching ? 'Reading…' : 'Refresh'}</span>
          </Button>
        }
      />

      {observation.error ? (
        <Card>
          <ErrorState error={observation.error} onRetry={observation.refetch} />
        </Card>
      ) : (
        <div className="space-y-4">
          <Window observation={observation} />

          <Section
            title="Traffic"
            description="Requests handled by this server, and how many of them failed."
          >
            <RateCard
              label="Requests"
              rate={observation.requests}
              total={current?.requests}
              unit="req/s"
            />
            <RateCard
              label="Failed requests"
              rate={observation.errors}
              total={current?.errors}
              unit="err/s"
              tone="danger"
              detail={
                current && current.requests > 0
                  ? `${formatRatio(current.errors, current.requests)} of all requests`
                  : undefined
              }
            />
          </Section>

          <Section title="Transfer" description="Object bytes moved through this server.">
            <RateCard
              label="Uploaded"
              rate={observation.uploadBytes}
              total={current?.upload_bytes}
              unit="/s"
              bytes
            />
            <RateCard
              label="Downloaded"
              rate={observation.downloadBytes}
              total={current?.download_bytes}
              unit="/s"
              bytes
            />
          </Section>

          <Section title="Storage" description="What is stored, and what it costs on disk.">
            <MetricCard
              label="Logical data"
              value={
                current ? (
                  formatBytes(current.storage.logical_bytes)
                ) : (
                  <Skeleton className="h-7 w-24" />
                )
              }
              detail={current ? `${formatCount(current.storage.object_count)} objects` : undefined}
            />
            <MetricCard
              label="Physical data"
              value={
                current ? (
                  formatBytes(current.storage.physical_bytes)
                ) : (
                  <Skeleton className="h-7 w-24" />
                )
              }
              detail={
                current && current.storage.logical_bytes > 0
                  ? `${formatRatio(current.storage.physical_bytes, current.storage.logical_bytes)} of logical`
                  : undefined
              }
            />
            <MetricCard
              label="Buckets"
              value={
                current ? (
                  formatCount(current.storage.bucket_count)
                ) : (
                  <Skeleton className="h-7 w-12" />
                )
              }
              detail={
                current
                  ? `${formatCount(current.storage.version_count)} versions retained`
                  : undefined
              }
            />
            <MetricCard
              label="In-progress uploads"
              value={
                current ? (
                  formatBytes(current.storage.multipart_bytes)
                ) : (
                  <Skeleton className="h-7 w-20" />
                )
              }
              detail="Multipart parts not yet completed"
            />
          </Section>

          {current?.cluster ? <ClusterSection cluster={current.cluster} /> : null}
        </div>
      )}
    </>
  );
}

function Window({ observation }: { readonly observation: ReturnType<typeof useMetricsSamples> }) {
  const { observedAt, windowSeconds } = observation;
  return (
    <Card>
      <CardContent className="flex flex-wrap items-center gap-x-6 gap-y-1.5 type-meta py-3">
        <span>
          Last read{' '}
          {observedAt ? (
            <time dateTime={observedAt.toISOString()} className="text-ink">
              {observedAt.toLocaleTimeString()}
            </time>
          ) : (
            '—'
          )}
        </span>
        <span>
          Observed window{' '}
          <span className="text-ink">
            {windowSeconds > 0 ? formatDuration(windowSeconds) : 'starting'}
          </span>
        </span>
      </CardContent>
    </Card>
  );
}

function Section({
  title,
  description,
  children,
}: {
  readonly title: string;
  readonly description: string;
  readonly children: React.ReactNode;
}) {
  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>{title}</CardTitle>
        <CardDescription>{description}</CardDescription>
      </CardHeader>
      <CardContent>
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">{children}</div>
      </CardContent>
    </Card>
  );
}

/**
 * One derived rate.
 *
 * Until two readings exist there is no rate, and the card says it is still
 * collecting rather than showing a zero that looks like idleness.
 */
function RateCard({
  label,
  rate,
  total,
  unit,
  bytes = false,
  tone = 'accent',
  detail,
}: {
  readonly label: string;
  readonly rate: Rate | null;
  readonly total: number | undefined;
  readonly unit: string;
  readonly bytes?: boolean;
  readonly tone?: 'accent' | 'danger';
  readonly detail?: string;
}) {
  const value =
    rate === null
      ? 'Collecting…'
      : bytes
        ? `${formatBytes(rate.perSecond)}${unit}`
        : `${rate.perSecond.toFixed(rate.perSecond < 10 ? 2 : 0)} ${unit}`;
  return (
    <MetricCard
      label={label}
      value={rate === null ? <span className="text-base text-ink-muted">{value}</span> : value}
      detail={
        detail ??
        (total === undefined
          ? undefined
          : `${bytes ? formatBytes(total) : formatCount(total)} total since start`)
      }
      footer={
        rate === null ? undefined : (
          <RateChart
            series={rate.series}
            label={label}
            tone={tone}
            format={(value) =>
              bytes ? `${formatBytes(value)}${unit}` : `${value.toFixed(2)} ${unit}`
            }
          />
        )
      }
    />
  );
}

function ClusterSection({
  cluster,
}: {
  readonly cluster: NonNullable<
    NonNullable<ReturnType<typeof useMetricsSamples>['current']>['cluster']
  >;
}) {
  return (
    <Card>
      <CardHeader className="flex-col items-start">
        <CardTitle>Cluster</CardTitle>
        <CardDescription>Durability and capacity across this cluster.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex flex-wrap gap-6">
          <div className="space-y-1">
            <p className="text-xs font-medium text-ink-muted">Cluster</p>
            <StatusBadge
              level={cluster.healthy ? 'healthy' : 'degraded'}
              label={cluster.healthy ? 'Healthy' : 'Degraded'}
            />
          </div>
          <div className="space-y-1">
            <p className="text-xs font-medium text-ink-muted">Metadata quorum</p>
            <StatusBadge
              level={cluster.quorum_writable ? 'healthy' : 'critical'}
              label={cluster.quorum_writable ? 'Writable' : 'No quorum'}
            />
          </div>
        </div>
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <MetricCard label="Nodes" value={formatCount(cluster.nodes)} />
          <MetricCard
            label="Under-replicated"
            value={formatCount(cluster.under_replicated_objects)}
            detail="Objects below their configured replica count"
          />
          <MetricCard
            label="Repairs running"
            value={formatCount(cluster.repair_active_tasks)}
            detail="Active reconstruction tasks"
          />
          <MetricCard
            label="This node's disk"
            value={formatBytes(cluster.node_used_bytes)}
            footer={
              <UsageBar
                used={cluster.node_used_bytes}
                total={cluster.node_capacity_bytes}
                label="Node disk utilisation"
              />
            }
          />
        </div>
      </CardContent>
    </Card>
  );
}
