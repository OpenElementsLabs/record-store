'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { createColumnHelper } from '@tanstack/react-table';
import { MoreHorizontal } from 'lucide-react';
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
  activateDevice,
  drainDevice,
  fetchClusterDevices,
  maintainDevice,
  releaseDevice,
  resumeDevice,
  retireDevice,
} from '@/lib/api/cluster';
import { ApiError } from '@/lib/api/error';
import { formatBytes, shortenIdentifier } from '@/lib/format';
import type { ClusterDevice, DeviceHealth, DeviceKind, DeviceState } from '@/types/cluster';

/** Maps a device lifecycle state onto a presentation level. */
function levelForState(state: DeviceState): StatusLevel {
  switch (state) {
    case 'active':
      return 'healthy';
    case 'discovered':
    case 'available':
      return 'pending';
    case 'degraded':
      return 'warning';
    case 'draining':
      return 'pending';
    case 'maintenance':
      return 'paused';
    case 'failed':
      return 'critical';
    case 'safe_to_remove':
      return 'healthy';
    case 'retired':
      return 'disabled';
    default:
      return 'unknown';
  }
}

/**
 * Maps observed health onto a presentation level.
 *
 * `unknown` and `unsupported` are shown as unknown rather than as healthy. The
 * platform not reporting health is not the same as reporting good health, and
 * dressing one up as the other is how an operator ends up trusting a drive
 * nothing has actually checked.
 */
function levelForHealth(health: DeviceHealth): StatusLevel {
  switch (health) {
    case 'healthy':
      return 'healthy';
    case 'degraded':
      return 'warning';
    case 'failed':
    case 'unavailable':
      return 'critical';
    default:
      return 'unknown';
  }
}

/** Human-facing name for a physical device kind. */
function kindLabel(kind: DeviceKind): string {
  switch (kind) {
    case 'nvme':
      return 'NVMe';
    case 'sata_ssd':
      return 'SATA SSD';
    case 'sas_ssd':
      return 'SAS SSD';
    case 'sata_hdd':
      return 'SATA HDD';
    case 'sas_hdd':
      return 'SAS HDD';
    case 'ssd':
      return 'SSD';
    case 'hdd':
      return 'HDD';
    case 'block_device':
      return 'Block device';
    case 'raid_logical_volume':
      return 'RAID volume';
    case 'cloud_block_volume':
      return 'Cloud volume';
    case 'filesystem_directory':
      return 'Directory';
    default:
      return 'Unknown';
  }
}

function stateLabel(state: DeviceState): string {
  return state === 'safe_to_remove'
    ? 'Safe to remove'
    : state.charAt(0).toUpperCase() + state.slice(1);
}

type DeviceActionKind = 'drain' | 'release' | 'retire';

type PendingAction = {
  readonly kind: DeviceActionKind;
  readonly device: ClusterDevice;
};

const column = createColumnHelper<DataTableFeatures, ClusterDevice>();

export function DrivesScreen() {
  const client = useQueryClient();
  const permissions = usePermissions();
  const [pending, setPending] = React.useState<PendingAction | null>(null);

  const devices = useQuery({
    queryKey: queryKeys.clusterDevices,
    queryFn: ({ signal }) => fetchClusterDevices(signal),
    refetchInterval: 15_000,
  });

  const invalidate = async () => {
    await client.invalidateQueries({ queryKey: queryKeys.clusterDevices });
    await client.invalidateQueries({ queryKey: queryKeys.clusterStatus });
  };

  const confirmed = useMutation({
    mutationFn: (request: PendingAction) => {
      const { node_id: node, device_id: id } = request.device;
      switch (request.kind) {
        case 'drain':
          return drainDevice(node, id);
        case 'release':
          return releaseDevice(node, id);
        case 'retire':
          return retireDevice(node, id);
      }
    },
    onSuccess: async (_result, request) => {
      toast.success(
        request.kind === 'release'
          ? 'Device marked safe to remove'
          : `Device ${request.kind} started`,
      );
      setPending(null);
      await invalidate();
    },
  });

  const immediate = useMutation({
    mutationFn: ({
      device,
      action,
    }: {
      device: ClusterDevice;
      action: 'activate' | 'maintenance' | 'resume';
    }) => {
      switch (action) {
        case 'activate':
          return activateDevice(device.node_id, device.device_id);
        case 'maintenance':
          return maintainDevice(device.node_id, device.device_id);
        case 'resume':
          return resumeDevice(device.node_id, device.device_id);
      }
    },
    onSuccess: async () => {
      toast.success('Device updated');
      await invalidate();
    },
    onError: (error) =>
      toast.error(error instanceof ApiError ? error.message : 'Could not update the device'),
  });

  const columns = React.useMemo(
    () =>
      column.columns([
        column.accessor((row) => row.device_id, {
          id: 'device',
          header: 'Device',
          cell: ({ row }) => (
            <div className="space-y-0.5">
              <p className="font-mono text-xs text-ink" title={row.original.device_id}>
                {shortenIdentifier(row.original.device_id, 8)}
              </p>
              {/* The path is descriptive only: it can change across reboots, so
                  it is never the thing an operator addresses a device by. */}
              <p className="type-meta-subtle">{row.original.current_path ?? 'path unknown'}</p>
            </div>
          ),
        }),
        column.accessor((row) => row.node_id, {
          id: 'node',
          header: 'Node',
          cell: ({ row }) => (
            <span className="font-mono text-xs" title={row.original.node_id}>
              {shortenIdentifier(row.original.node_id, 8)}
            </span>
          ),
        }),
        column.accessor((row) => row.kind, {
          id: 'kind',
          header: 'Type',
          cell: ({ row }) => (
            <div className="flex flex-wrap gap-1">
              <Badge tone="neutral">{kindLabel(row.original.kind)}</Badge>
              <Badge tone="neutral">{row.original.storage_class}</Badge>
            </div>
          ),
        }),
        column.accessor((row) => row.state, {
          id: 'state',
          header: 'Lifecycle',
          cell: ({ row }) => (
            <div className="space-y-1">
              <StatusBadge
                level={levelForState(row.original.state)}
                label={stateLabel(row.original.state)}
              />
              {row.original.accepts_placement ? null : (
                <p className="type-meta-subtle">no new data</p>
              )}
            </div>
          ),
        }),
        column.accessor((row) => row.health, {
          id: 'health',
          header: 'Health',
          cell: ({ row }) => (
            <StatusBadge
              level={levelForHealth(row.original.health)}
              label={row.original.health.charAt(0).toUpperCase() + row.original.health.slice(1)}
            />
          ),
        }),
        column.accessor((row) => row.utilization_percent, {
          id: 'capacity',
          header: 'Used',
          cell: ({ row }) => (
            <div className="w-32 space-y-1">
              <p className="text-xs tabular-nums text-ink">
                {formatBytes(row.original.usable_bytes - row.original.available_bytes)} of{' '}
                {formatBytes(row.original.usable_bytes)}
              </p>
              <UsageBar
                used={row.original.usable_bytes - row.original.available_bytes}
                total={row.original.usable_bytes}
                label={`Utilisation for device ${row.original.device_id}`}
              />
            </div>
          ),
        }),
        column.display({
          id: 'actions',
          header: () => <span className="sr-only">Actions</span>,
          cell: ({ row }) => {
            if (!permissions.manage_cluster) return null;
            const device = row.original;
            const resumable = device.state === 'draining' || device.state === 'maintenance';
            return (
              <div className="flex justify-end">
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      aria-label={`Actions for device ${device.device_id}`}
                    >
                      <MoreHorizontal aria-hidden />
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent>
                    {device.state === 'available' ? (
                      <DropdownMenuItem
                        onSelect={() => immediate.mutate({ device, action: 'activate' })}
                      >
                        Activate
                      </DropdownMenuItem>
                    ) : null}
                    <DropdownMenuItem onSelect={() => setPending({ kind: 'drain', device })}>
                      Drain device
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      onSelect={() => immediate.mutate({ device, action: 'maintenance' })}
                    >
                      Enter maintenance
                    </DropdownMenuItem>
                    {resumable ? (
                      <DropdownMenuItem
                        onSelect={() => immediate.mutate({ device, action: 'resume' })}
                      >
                        Resume device
                      </DropdownMenuItem>
                    ) : null}
                    <DropdownMenuSeparator />
                    {/* Release asks the server whether removal is safe. It is
                        offered even for a device that looks empty, because only
                        the server can answer that question. */}
                    <DropdownMenuItem onSelect={() => setPending({ kind: 'release', device })}>
                      Check if safe to remove
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      destructive
                      onSelect={() => setPending({ kind: 'retire', device })}
                    >
                      Retire device
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            );
          },
        }),
      ]),
    [immediate, permissions.manage_cluster],
  );

  return (
    <>
      <PageHeader
        title="Drives"
        description="Every storage device the cluster places data on, with its lifecycle state, observed health, and capacity."
      />

      <Card>
        {devices.isError ? (
          <ErrorState error={devices.error} onRetry={() => void devices.refetch()} />
        ) : (
          <DataTable
            data={devices.data ?? []}
            columns={columns}
            rowId={(device) => device.device_id}
            loading={devices.isPending}
            empty={
              <EmptyState
                title="No devices registered"
                description="Devices appear here once a node registers one. Discovering a disk never enrolls it."
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
            confirmed.reset();
          }
        }}
        strength={pending?.kind === 'retire' ? 'type-to-confirm' : 'acknowledge'}
        expectedText={pending?.kind === 'retire' ? pending.device.device_id.slice(0, 8) : undefined}
        title={titleFor(pending)}
        description={descriptionFor(pending)}
        consequence={consequenceFor(pending)}
        confirmLabel={confirmLabelFor(pending)}
        pending={confirmed.isPending}
        error={confirmed.error}
        onConfirm={() => {
          if (pending) confirmed.mutate(pending);
        }}
      />
    </>
  );
}

function titleFor(pending: PendingAction | null): string {
  if (!pending) return '';
  const id = shortenIdentifier(pending.device.device_id, 8);
  switch (pending.kind) {
    case 'drain':
      return `Drain device ${id}?`;
    case 'release':
      return `Is device ${id} safe to remove?`;
    case 'retire':
      return `Retire device ${id}?`;
  }
}

function descriptionFor(pending: PendingAction | null): string {
  if (!pending) return '';
  switch (pending.kind) {
    case 'drain':
      return 'The device stops receiving new data and its replicas are moved to other devices.';
    case 'release':
      return 'Record Store checks whether the device still holds replicas. It is only marked safe to remove if it holds none.';
    case 'retire':
      return 'The device is permanently removed from placement and management.';
  }
}

function consequenceFor(pending: PendingAction | null): string | undefined {
  if (!pending) return undefined;
  switch (pending.kind) {
    case 'drain':
      return 'Existing replicas keep serving reads and keep counting toward durability until they have been copied elsewhere.';
    case 'release':
      return 'This is refused while the device still owns replicas, so a success means evacuation actually finished.';
    case 'retire':
      return 'This cannot be undone. Retire only a device that has already been drained and physically removed.';
  }
}

function confirmLabelFor(pending: PendingAction | null): string {
  if (!pending) return 'Confirm';
  switch (pending.kind) {
    case 'drain':
      return 'Drain device';
    case 'release':
      return 'Check';
    case 'retire':
      return 'Retire device';
  }
}
