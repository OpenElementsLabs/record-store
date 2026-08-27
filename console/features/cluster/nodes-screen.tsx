'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { createColumnHelper } from '@tanstack/react-table';
import { MoreHorizontal } from 'lucide-react';
import Link from 'next/link';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTable, type DataTableFeatures } from '@/components/data-table';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge, type StatusLevel } from '@/components/status-badge';
import { UsageBar } from '@/components/metric-card';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import {
  decommissionNode,
  drainNode,
  fetchClusterNodes,
  maintainNode,
  resumeNode,
} from '@/lib/api/cluster';
import { ApiError } from '@/lib/api/error';
import { formatBytes, formatCount, formatRelativeTime, shortenIdentifier } from '@/lib/format';
import type { ClusterNode, NodeState } from '@/types/cluster';

/** Maps a node lifecycle state onto a presentation level. */
function levelFor(state: NodeState): StatusLevel {
  switch (state) {
    case 'healthy':
      return 'healthy';
    case 'joining':
      return 'pending';
    case 'suspect':
      return 'warning';
    case 'unreachable':
    case 'offline':
      return 'critical';
    case 'draining':
      return 'pending';
    case 'maintenance':
      return 'paused';
    case 'decommissioned':
      return 'disabled';
    default:
      return 'unknown';
  }
}

type PendingAction =
  | { readonly kind: 'drain'; readonly node: ClusterNode }
  | { readonly kind: 'maintenance'; readonly node: ClusterNode }
  | { readonly kind: 'decommission'; readonly node: ClusterNode };

const column = createColumnHelper<DataTableFeatures, ClusterNode>();

export function NodesScreen() {
  const client = useQueryClient();
  const permissions = usePermissions();
  const [pending, setPending] = React.useState<PendingAction | null>(null);

  const nodes = useQuery({
    queryKey: queryKeys.clusterNodes,
    queryFn: ({ signal }) => fetchClusterNodes(signal),
    refetchInterval: 15_000,
  });

  const invalidate = async () => {
    await client.invalidateQueries({ queryKey: queryKeys.clusterNodes });
    await client.invalidateQueries({ queryKey: queryKeys.clusterStatus });
  };

  const action = useMutation({
    mutationFn: async (request: PendingAction) => {
      switch (request.kind) {
        case 'drain':
          return drainNode(request.node.node_id);
        case 'maintenance':
          return maintainNode(request.node.node_id);
        case 'decommission':
          // Force is never sent from the console. If the backend refuses on
          // durability grounds, that refusal is shown to the operator.
          return decommissionNode(request.node.node_id, false);
      }
    },
    onSuccess: async (_result, request) => {
      toast.success(`Node ${request.kind} started`);
      setPending(null);
      await invalidate();
    },
  });

  const resume = useMutation({
    mutationFn: (node: ClusterNode) => resumeNode(node.node_id),
    onSuccess: async () => {
      toast.success('Node resumed');
      await invalidate();
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not resume the node'),
  });

  const columns = React.useMemo(
    () =>
      column.columns([
        column.accessor((row) => row.node_id, {
          id: 'node',
          header: 'Node',
          cell: ({ row }) => (
            <div className="space-y-0.5">
              <Link
                className="font-mono text-xs text-accent hover:underline"
                href={`/cluster/nodes/${encodeURIComponent(row.original.node_id)}`}
                title={row.original.node_id}
              >
                {shortenIdentifier(row.original.node_id, 8)}
              </Link>
              <p className="type-meta-subtle">{row.original.rpc_address}</p>
            </div>
          ),
        }),
        column.accessor((row) => row.state, {
          id: 'state',
          header: 'Status',
          cell: ({ row }) => (
            <div className="space-y-1">
              <StatusBadge
                level={levelFor(row.original.state)}
                label={capitalise(row.original.state)}
              />
              {row.original.state_reason ? (
                <p className="max-w-48 type-meta-subtle">{row.original.state_reason}</p>
              ) : null}
            </div>
          ),
        }),
        column.accessor((row) => row.utilization_percent, {
          id: 'capacity',
          header: 'Used',
          cell: ({ row }) => (
            <div className="w-32 space-y-1">
              <p className="text-xs tabular-nums text-ink">
                {formatBytes(row.original.capacity_bytes - row.original.available_bytes)} of{' '}
                {formatBytes(row.original.capacity_bytes)}
              </p>
              <UsageBar
                used={row.original.capacity_bytes - row.original.available_bytes}
                total={row.original.capacity_bytes}
                label={`Utilisation for ${row.original.node_id}`}
              />
            </div>
          ),
        }),
        column.accessor((row) => row.replicas, {
          id: 'replicas',
          header: 'Replicas',
          cell: ({ getValue }) => (
            <span className="tabular-nums text-xs">{formatCount(getValue())}</span>
          ),
        }),
        column.accessor((row) => row.storage_class, {
          id: 'topology',
          header: 'Topology',
          cell: ({ row }) => (
            <div className="flex flex-wrap gap-1">
              <Badge tone="neutral">{row.original.storage_class}</Badge>
              {Object.entries(row.original.failure_domain).map(([key, value]) => (
                <Badge key={key} tone="neutral" className="font-mono">
                  {key}={value}
                </Badge>
              ))}
              {row.original.metadata_voter ? <Badge tone="accent">voter</Badge> : null}
            </div>
          ),
        }),
        column.accessor((row) => row.software_version, {
          id: 'version',
          header: 'Version',
          cell: ({ row }) => (
            <div className="space-y-0.5">
              <p className="font-mono text-xs text-ink">{row.original.software_version}</p>
              <p className="type-meta-subtle">
                seen {formatRelativeTime(row.original.last_heartbeat_at)}
              </p>
            </div>
          ),
        }),
        column.display({
          id: 'actions',
          header: () => <span className="sr-only">Actions</span>,
          cell: ({ row }) => {
            if (!permissions.manage_cluster) return null;
            const node = row.original;
            const resumable = node.state === 'draining' || node.state === 'maintenance';
            return (
              <div className="flex justify-end">
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      aria-label={`Actions for node ${node.node_id}`}
                    >
                      <MoreHorizontal aria-hidden />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent>
                    <DropdownMenuItem onSelect={() => setPending({ kind: 'drain', node })}>
                      Drain node
                    </DropdownMenuItem>
                    <DropdownMenuItem onSelect={() => setPending({ kind: 'maintenance', node })}>
                      Enter maintenance
                    </DropdownMenuItem>
                    {resumable ? (
                      <DropdownMenuItem onSelect={() => resume.mutate(node)}>
                        Resume node
                      </DropdownMenuItem>
                    ) : null}
                    <DropdownMenuSeparator />
                    <DropdownMenuItem
                      destructive
                      onSelect={() => setPending({ kind: 'decommission', node })}
                    >
                      Decommission node
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            );
          },
        }),
      ]),
    [permissions.manage_cluster, resume],
  );

  return (
    <>
      <PageHeader
        title="Nodes"
        description="Storage nodes, their lifecycle state, capacity, and topology labels."
      />

      <Card>
        {nodes.isError ? (
          <ErrorState error={nodes.error} onRetry={() => void nodes.refetch()} />
        ) : (
          <DataTable
            data={nodes.data ?? []}
            columns={columns}
            rowId={(node) => node.node_id}
            loading={nodes.isPending}
            empty={
              <EmptyState
                title="No nodes registered"
                description="Nodes appear here once they join the cluster."
              />
            }
          />
        )}
      </Card>

      <ConfirmDialog
        open={pending !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPending(null);
            action.reset();
          }
        }}
        strength={pending?.kind === 'decommission' ? 'type-to-confirm' : 'acknowledge'}
        expectedText={
          pending?.kind === 'decommission' ? pending.node.node_id.slice(0, 8) : undefined
        }
        title={titleFor(pending)}
        description={descriptionFor(pending)}
        consequence={consequenceFor(pending)}
        confirmLabel={confirmLabelFor(pending)}
        pending={action.isPending}
        error={action.error}
        onConfirm={() => {
          if (pending) action.mutate(pending);
        }}
      />
    </>
  );
}

function titleFor(pending: PendingAction | null): string {
  if (!pending) return '';
  const id = shortenIdentifier(pending.node.node_id, 8);
  switch (pending.kind) {
    case 'drain':
      return `Drain node ${id}?`;
    case 'maintenance':
      return `Put node ${id} into maintenance?`;
    case 'decommission':
      return `Decommission node ${id}?`;
  }
}

function descriptionFor(pending: PendingAction | null): string {
  if (!pending) return '';
  switch (pending.kind) {
    case 'drain':
      return 'The node stops receiving new replicas and its existing replicas are moved elsewhere.';
    case 'maintenance':
      return 'The node keeps its data and stops receiving new replicas. Nothing is moved.';
    case 'decommission':
      return 'The node is permanently removed from the cluster once its replicas have moved.';
  }
}

function consequenceFor(pending: PendingAction | null): string | undefined {
  if (!pending) return undefined;
  switch (pending.kind) {
    case 'drain':
      return `${formatCount(pending.node.replicas)} replica(s) will be copied to other nodes before the node is safe to stop.`;
    case 'maintenance':
      return undefined;
    case 'decommission':
      return 'Record Store refuses this if it would drop object versions below their required durability. The refusal will be shown here rather than overridden.';
  }
}

function confirmLabelFor(pending: PendingAction | null): string {
  if (!pending) return 'Confirm';
  switch (pending.kind) {
    case 'drain':
      return 'Start drain';
    case 'maintenance':
      return 'Enter maintenance';
    case 'decommission':
      return 'Decommission node';
  }
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
