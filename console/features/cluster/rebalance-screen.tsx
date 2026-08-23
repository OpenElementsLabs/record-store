'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { fetchRebalanceOperations, startRebalance } from '@/lib/api/cluster';
import { ApiError } from '@/lib/api/error';
import { formatBytes, formatCount, formatDateTime } from '@/lib/format';
import type { ClusterOperation, ClusterOperationState } from '@/types/cluster';

/** Whether an operation is still doing work. */
function isRunning(state: ClusterOperationState): boolean {
  return state === 'planning' || state === 'moving' || state === 'verifying';
}

const STATE_LEVEL: Record<ClusterOperationState, 'healthy' | 'paused' | 'critical' | 'disabled'> = {
  planning: 'paused',
  moving: 'paused',
  verifying: 'paused',
  completed: 'healthy',
  cancelled: 'disabled',
  failed: 'critical',
};

/**
 * Replica rebalancing.
 *
 * Rebalancing evens out capacity by moving replicas; it does not create them.
 * It is throttled and interruptible, and it is deliberately lower priority than
 * repair, so a cluster that is both under-replicated and unbalanced fixes
 * durability first.
 */
export function RebalanceScreen() {
  const client = useQueryClient();
  const permissions = usePermissions();
  const [confirming, setConfirming] = React.useState(false);

  const operations = useQuery({
    queryKey: queryKeys.clusterRebalance,
    queryFn: ({ signal }) => fetchRebalanceOperations(signal),
    refetchInterval: 15_000,
  });

  const start = useMutation({
    mutationFn: () => startRebalance(),
    onSuccess: async () => {
      toast.success('Rebalance requested');
      setConfirming(false);
      await client.invalidateQueries({ queryKey: queryKeys.clusterRebalance });
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not start a rebalance'),
  });

  const all = operations.data ?? [];
  const active = all.filter((operation) => isRunning(operation.state));
  const finished = all.filter((operation) => !isRunning(operation.state));

  return (
    <>
      <PageHeader
        title="Rebalancing"
        description="Moves replicas between nodes to even out capacity. It never changes how many copies exist."
        actions={
          permissions.manage_cluster ? (
            <Button
              variant="primary"
              size="sm"
              disabled={active.length > 0 || start.isPending}
              onClick={() => setConfirming(true)}
            >
              Start rebalance
            </Button>
          ) : null
        }
      />

      {operations.isError ? (
        <Card>
          <ErrorState error={operations.error} onRetry={() => void operations.refetch()} />
        </Card>
      ) : operations.isPending ? (
        <Card>
          <CardContent className="py-6">
            <Skeleton className="h-20 w-full" />
          </CardContent>
        </Card>
      ) : (
        <div className="space-y-4">
          {active.length > 0 ? (
            <Card>
              <CardHeader className="flex-col items-start">
                <CardTitle>In progress</CardTitle>
                <CardDescription>
                  Movement is throttled by the cluster&apos;s configured limits, so a large
                  rebalance takes time by design.
                </CardDescription>
              </CardHeader>
              <CardContent className="space-y-4">
                {active.map((operation) => (
                  <OperationRow key={operation.id} operation={operation} />
                ))}
              </CardContent>
            </Card>
          ) : (
            <Card>
              <CardContent className="py-4">
                <p className="text-sm text-ink-muted">No rebalance is running.</p>
              </CardContent>
            </Card>
          )}

          <Card>
            <CardHeader className="flex-col items-start">
              <CardTitle>History</CardTitle>
              <CardDescription>Rebalance operations this cluster has recorded.</CardDescription>
            </CardHeader>
            <CardContent>
              {finished.length === 0 ? (
                <EmptyState
                  title="No completed rebalances"
                  description="Nothing has been rebalanced on this cluster yet."
                />
              ) : (
                <ul className="divide-y divide-border">
                  {finished.map((operation) => (
                    <li key={operation.id} className="py-3">
                      <OperationRow operation={operation} />
                    </li>
                  ))}
                </ul>
              )}
            </CardContent>
          </Card>
        </div>
      )}

      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title="Start a rebalance"
        description="OES will move replicas between nodes to even out capacity. Movement is throttled and can be left running; it does not change how many copies of an object exist."
        confirmLabel="Start rebalance"
        strength="acknowledge"
        pending={start.isPending}
        error={start.error}
        onConfirm={() => start.mutate()}
      />
    </>
  );
}

/**
 * One operation, with progress only where it can be computed.
 *
 * A percentage needs a known total. While planning, nothing has been counted
 * yet, so the row reports what has moved instead of a fraction that would be
 * guesswork.
 */
function OperationRow({ operation }: { readonly operation: ClusterOperation }) {
  const { progress } = operation;
  const total = progress.bytes_moved + progress.bytes_remaining;
  const percent = total > 0 ? Math.round((progress.bytes_moved / total) * 100) : null;

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
        <StatusBadge level={STATE_LEVEL[operation.state]} label={capitalise(operation.state)} />
        <span className="text-xs text-ink-muted">
          started{' '}
          <time dateTime={operation.started_at}>{formatDateTime(operation.started_at)}</time>
        </span>
        {operation.completed_at ? (
          <span className="text-xs text-ink-muted">
            finished{' '}
            <time dateTime={operation.completed_at}>{formatDateTime(operation.completed_at)}</time>
          </span>
        ) : null}
        {progress.replicas_moving > 0 ? (
          <span className="text-xs text-ink-subtle">
            {formatCount(progress.replicas_moving)} transfers in flight
          </span>
        ) : null}
      </div>

      {percent === null ? (
        <p className="text-xs text-ink-subtle">
          {isRunning(operation.state)
            ? 'Planning: the amount to move has not been counted yet.'
            : 'Nothing needed moving.'}
        </p>
      ) : (
        <>
          <div
            className="h-1.5 w-full overflow-hidden rounded-full bg-surface-muted"
            role="progressbar"
            aria-label={`Rebalance progress for operation ${operation.id}`}
            aria-valuenow={percent}
            aria-valuemin={0}
            aria-valuemax={100}
          >
            <div className="h-full bg-accent" style={{ width: `${percent}%` }} />
          </div>
          <p className="text-xs tabular-nums text-ink-muted">
            {formatBytes(progress.bytes_moved)} of {formatBytes(total)} moved ·{' '}
            {formatCount(progress.objects_moved)} objects
            {progress.objects_remaining > 0
              ? `, ${formatCount(progress.objects_remaining)} remaining`
              : ''}
          </p>
        </>
      )}

      {progress.tasks_parked > 0 ? (
        <p className="text-xs text-warn">
          {formatCount(progress.tasks_parked)} transfers parked after exhausting their retries.
        </p>
      ) : null}
      {operation.message ? <p className="text-xs text-ink-muted">{operation.message}</p> : null}
    </div>
  );
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
