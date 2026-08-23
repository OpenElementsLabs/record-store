'use client';

import { useQuery } from '@tanstack/react-query';
import { RefreshCw } from 'lucide-react';

import { ErrorState } from '@/components/error-state';
import { MetricCard } from '@/components/metric-card';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { queryKeys } from '@/hooks/use-system';
import { fetchClusterStatus, fetchRepairStatus } from '@/lib/api/cluster';
import { formatBytes, formatCount, formatRatio } from '@/lib/format';

/**
 * Replication health and the repair queue that restores it.
 *
 * These are one concern: under-replication is the problem and repair is the
 * mechanism that clears it, so watching them apart invites reading a healthy
 * queue as healthy durability.
 */
export function DurabilityScreen() {
  const status = useQuery({
    queryKey: queryKeys.clusterStatus,
    queryFn: ({ signal }) => fetchClusterStatus(signal),
    refetchInterval: 30_000,
  });
  const repair = useQuery({
    queryKey: queryKeys.clusterRepair,
    queryFn: ({ signal }) => fetchRepairStatus(signal),
    refetchInterval: 30_000,
  });

  const replication = status.data?.replication;
  const queue = repair.data;
  const refreshing = status.isFetching || repair.isFetching;

  return (
    <>
      <PageHeader
        title="Durability"
        description="How many copies of your data exist, and what OES is doing about any shortfall."
        actions={
          <Button
            size="sm"
            aria-label="Refresh durability"
            disabled={refreshing}
            onClick={() => {
              void status.refetch();
              void repair.refetch();
            }}
          >
            <RefreshCw aria-hidden className={refreshing ? 'animate-spin' : ''} />
            <span aria-hidden>{refreshing ? 'Reading…' : 'Refresh'}</span>
          </Button>
        }
      />

      {status.isError ? (
        <Card>
          <ErrorState error={status.error} onRetry={() => void status.refetch()} />
        </Card>
      ) : (
        <div className="space-y-4">
          <Card>
            <CardContent className="flex flex-wrap items-center gap-6">
              <div className="space-y-1">
                <p className="text-xs font-medium text-ink-muted">Data health</p>
                {status.data ? (
                  <StatusBadge
                    level={status.data.data.health}
                    label={capitalise(status.data.data.health)}
                  />
                ) : (
                  <Skeleton className="h-5 w-20" />
                )}
              </div>
              <div className="space-y-1">
                <p className="text-xs font-medium text-ink-muted">Configured copies</p>
                <p className="type-body">
                  {replication
                    ? `${replication.replication_factor} replicas, ${replication.required_acknowledgements} acknowledged before a write succeeds`
                    : '—'}
                </p>
              </div>
            </CardContent>
          </Card>

          <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
            <MetricCard
              label="Payloads"
              value={
                replication ? formatCount(replication.payloads) : <Skeleton className="h-7 w-16" />
              }
              detail={replication ? formatBytes(replication.logical_bytes) : undefined}
            />
            <MetricCard
              label="Under-replicated"
              value={
                replication ? (
                  formatCount(replication.under_replicated_payloads)
                ) : (
                  <Skeleton className="h-7 w-16" />
                )
              }
              detail="Fewer copies than configured"
            />
            <MetricCard
              label="Unavailable"
              value={
                replication ? (
                  formatCount(replication.unavailable_payloads)
                ) : (
                  <Skeleton className="h-7 w-16" />
                )
              }
              detail="No readable copy"
            />
            <MetricCard
              label="Storage amplification"
              value={
                replication && replication.logical_bytes > 0 ? (
                  formatRatio(replication.physical_bytes, replication.logical_bytes)
                ) : replication ? (
                  '—'
                ) : (
                  <Skeleton className="h-7 w-16" />
                )
              }
              detail={
                replication ? `${formatBytes(replication.physical_bytes)} on disk` : undefined
              }
            />
          </div>

          {replication && replication.unavailable_payloads > 0 ? (
            <Card>
              <CardContent>
                <p className="text-sm text-danger">
                  {formatCount(replication.unavailable_payloads)} payloads have no readable copy.
                  Repair cannot rebuild these from redundancy, because there is none left to read.
                </p>
              </CardContent>
            </Card>
          ) : null}

          <Card>
            <CardHeader className="flex-col items-start">
              <CardTitle>Repair queue</CardTitle>
              <CardDescription>
                Repair copies payloads back up to their configured replica count. It is throttled
                deliberately, so a large shortfall clears over time rather than saturating the
                network.
              </CardDescription>
            </CardHeader>
            <CardContent>
              {repair.isError ? (
                <ErrorState error={repair.error} onRetry={() => void repair.refetch()} />
              ) : (
                <div className="grid gap-4 sm:grid-cols-2">
                  <MetricCard
                    label="Tasks running"
                    value={
                      queue ? formatCount(queue.active_tasks) : <Skeleton className="h-7 w-12" />
                    }
                  />
                  <MetricCard
                    label="Tasks parked"
                    value={
                      queue ? formatCount(queue.parked_tasks) : <Skeleton className="h-7 w-12" />
                    }
                    detail="Stopped after exhausting their retries"
                  />
                </div>
              )}
              {queue && queue.parked_tasks > 0 ? (
                <p className="mt-3 text-sm text-warn">
                  Parked tasks will not retry on their own. They usually mean no eligible target had
                  room, or a source replica could not be read.
                </p>
              ) : null}
              {/*
                The backend exposes running and parked counts for repair, not a
                per-job list, bytes remaining, or throughput. Inventing those
                figures here would make the screen look more capable than the
                data behind it.
              */}
              <p className="mt-3 type-meta-subtle">
                OES reports repair as queue counts. Per-job detail and throughput are not exposed by
                the management API.
              </p>
            </CardContent>
          </Card>
        </div>
      )}
    </>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
