'use client';

import { useQuery } from '@tanstack/react-query';
import * as React from 'react';

import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Badge } from '@/components/ui/badge';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { queryKeys } from '@/hooks/use-system';
import { fetchClusterNodes } from '@/lib/api/cluster';
import { formatBytes, shortenIdentifier } from '@/lib/format';
import type { ClusterDevice, ClusterNode } from '@/types/cluster';

/**
 * Topology levels, outermost first.
 *
 * A deployment labels only the levels it actually has. One that labels nothing
 * still has nodes and devices, which is a valid topology rather than a
 * misconfiguration.
 */
const LEVELS = ['region', 'datacenter', 'zone', 'rack'] as const;

type Level = (typeof LEVELS)[number];

type TreeNode = {
  readonly label: string;
  readonly level: Level | null;
  /** True when no node at this point carried the label. */
  readonly unlabelled: boolean;
  readonly children: TreeNode[];
  readonly nodes: ClusterNode[];
};

/**
 * Groups nodes into whatever hierarchy their labels describe.
 *
 * Levels nobody labelled are skipped rather than rendered as a row of "unknown"
 * containers, because an unlabelled deployment is not a broken one.
 */
function buildTree(nodes: readonly ClusterNode[], levels: readonly Level[]): TreeNode[] {
  if (levels.length === 0) {
    return [];
  }
  const level = levels[0];
  const rest = levels.slice(1);
  if (level === undefined) {
    return [];
  }
  // Nobody labelled this level anywhere, so it is not part of this topology.
  if (!nodes.some((node) => node.failure_domain[level])) {
    return buildTree(nodes, rest);
  }

  const groups = new Map<string, ClusterNode[]>();
  for (const node of nodes) {
    const value = node.failure_domain[level] ?? '';
    const existing = groups.get(value);
    if (existing) existing.push(node);
    else groups.set(value, [node]);
  }

  return [...groups.entries()]
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([value, grouped]) => {
      const children = buildTree(grouped, rest);
      return {
        label: value === '' ? `No ${level}` : `${level} ${value}`,
        level,
        unlabelled: value === '',
        children,
        // Nodes hang off the deepest labelled level.
        nodes: children.length === 0 ? grouped : [],
      };
    });
}

function Devices({ devices }: { devices: readonly ClusterDevice[] }) {
  if (devices.length === 0) {
    return <p className="type-meta-subtle">No devices registered</p>;
  }
  return (
    <ul className="space-y-1">
      {devices.map((device) => (
        <li key={device.device_id} className="flex flex-wrap items-center gap-2">
          <span className="font-mono type-meta" title={device.device_id}>
            {shortenIdentifier(device.device_id, 6)}
          </span>
          <Badge tone="neutral">{device.storage_class}</Badge>
          <span className="type-meta-subtle">
            {formatBytes(device.usable_bytes - device.available_bytes)} of{' '}
            {formatBytes(device.usable_bytes)}
          </span>
          {device.accepts_placement ? null : (
            <StatusBadge level="paused" label={device.state.replace(/_/g, ' ')} />
          )}
        </li>
      ))}
    </ul>
  );
}

function Nodes({ nodes }: { nodes: readonly ClusterNode[] }) {
  return (
    <ul className="space-y-3">
      {nodes.map((node) => (
        <li key={node.node_id} className="space-y-1.5">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-mono text-xs text-ink" title={node.node_id}>
              {shortenIdentifier(node.node_id, 8)}
            </span>
            <span className="type-meta-subtle">{node.rpc_address}</span>
            <Badge tone="neutral">{node.state}</Badge>
          </div>
          <div className="border-l border-border pl-3">
            <Devices devices={node.devices ?? []} />
          </div>
        </li>
      ))}
    </ul>
  );
}

function Branch({ branch }: { branch: TreeNode }) {
  return (
    <li className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-medium text-ink">{branch.label}</span>
        {/* Unlabelled nodes are not proven to be separate, and placement treats
            them as one domain. Saying so here stops the tree from implying
            separation the cluster does not have. */}
        {branch.unlabelled ? <span className="type-meta-subtle">not proven separate</span> : null}
      </div>
      <div className="border-l border-border pl-3">
        {branch.children.length > 0 ? (
          <ul className="space-y-2">
            {branch.children.map((child) => (
              <Branch key={`${child.level}:${child.label}`} branch={child} />
            ))}
          </ul>
        ) : (
          <Nodes nodes={branch.nodes} />
        )}
      </div>
    </li>
  );
}

export function TopologyScreen() {
  const nodes = useQuery({
    queryKey: queryKeys.clusterNodes,
    queryFn: ({ signal }) => fetchClusterNodes(signal),
    refetchInterval: 30_000,
  });

  const tree = React.useMemo(() => buildTree(nodes.data ?? [], LEVELS), [nodes.data]);

  return (
    <>
      <PageHeader
        title="Topology"
        description="How this cluster is laid out: the failure domains its nodes declare, and the devices under them."
      />

      <Card className="p-4">
        {nodes.isError ? (
          <ErrorState error={nodes.error} onRetry={() => void nodes.refetch()} />
        ) : nodes.isPending ? (
          <Skeleton className="h-32 w-full" />
        ) : (nodes.data?.length ?? 0) === 0 ? (
          <EmptyState
            title="No nodes registered"
            description="Nodes appear here once they join the cluster."
          />
        ) : tree.length === 0 ? (
          // No level is labelled anywhere, so there is no hierarchy to draw.
          <div className="space-y-3">
            <p className="type-meta-subtle">
              No topology labels are configured, so every node is its own group. Set
              <code className="mx-1">cluster.failure_domain</code>
              to describe racks, zones, or regions.
            </p>
            <Nodes nodes={nodes.data ?? []} />
          </div>
        ) : (
          <ul className="space-y-3">
            {tree.map((branch) => (
              <Branch key={`${branch.level}:${branch.label}`} branch={branch} />
            ))}
          </ul>
        )}
      </Card>
    </>
  );
}
