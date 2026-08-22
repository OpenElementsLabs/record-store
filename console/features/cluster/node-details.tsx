'use client';

import { useQuery } from '@tanstack/react-query';

import { Breadcrumbs } from '@/components/breadcrumbs';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { StatusBadge } from '@/components/status-badge';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { queryKeys } from '@/hooks/use-system';
import { fetchClusterNode } from '@/lib/api/cluster';
import { formatBytes, formatDateTime, shortenIdentifier } from '@/lib/format';
import type { NodeState } from '@/types/cluster';

/** Operational details for one real cluster node. */
export function NodeDetails({ nodeId }: { readonly nodeId: string }) {
  const node = useQuery({
    queryKey: queryKeys.clusterNode(nodeId),
    queryFn: ({ signal }) => fetchClusterNode(nodeId, signal),
    refetchInterval: 15_000,
  });

  return (
    <>
      <Breadcrumbs
        items={[
          { label: 'Cluster', href: '/cluster' },
          { label: 'Nodes', href: '/cluster/nodes' },
          { label: shortenIdentifier(nodeId, 8) },
        ]}
      />
      <PageHeader
        title={`Node ${shortenIdentifier(nodeId, 8)}`}
        description="Identity, capacity, topology, and membership state reported by this node."
      />

      {node.isError ? (
        <Card>
          <ErrorState error={node.error} onRetry={() => void node.refetch()} />
        </Card>
      ) : (
        <Card>
          <CardHeader>
            <CardTitle>Node details</CardTitle>
          </CardHeader>
          <CardContent>
            {node.data ? (
              <dl className="grid gap-x-8 gap-y-4 sm:grid-cols-2">
                <Row label="Node ID" value={node.data.node_id} mono />
                <Row label="RPC address" value={node.data.rpc_address} mono />
                <div className="space-y-1">
                  <dt className="text-xs text-ink-muted">Status</dt>
                  <dd>
                    <StatusBadge
                      level={levelFor(node.data.state)}
                      label={capitalise(node.data.state)}
                    />
                  </dd>
                </div>
                <Row label="Software version" value={node.data.software_version} mono />
                <Row label="Storage class" value={node.data.storage_class} />
                <Row
                  label="Capacity"
                  value={`${formatBytes(node.data.capacity_bytes - node.data.available_bytes)} used of ${formatBytes(node.data.capacity_bytes)}`}
                />
                <Row label="Replicas" value={node.data.replicas.toLocaleString()} />
                <Row
                  label="Metadata membership"
                  value={node.data.metadata_voter ? 'Voting member' : 'Non-voter'}
                />
                <Row
                  label="Last heartbeat"
                  value={
                    node.data.last_heartbeat_at
                      ? formatDateTime(node.data.last_heartbeat_at)
                      : 'Not observed'
                  }
                />
                <div className="space-y-1 sm:col-span-2">
                  <dt className="text-xs text-ink-muted">Failure domain</dt>
                  <dd className="flex flex-wrap gap-1">
                    {Object.entries(node.data.failure_domain).map(([key, value]) => (
                      <Badge key={key} tone="neutral" className="font-mono">
                        {key}={value}
                      </Badge>
                    ))}
                  </dd>
                </div>
                {node.data.state_reason ? (
                  <Row label="State reason" value={node.data.state_reason} />
                ) : null}
              </dl>
            ) : (
              <Skeleton className="h-64 w-full" />
            )}
          </CardContent>
        </Card>
      )}
    </>
  );
}

function Row({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="min-w-0 space-y-1">
      <dt className="text-xs text-ink-muted">{label}</dt>
      <dd className={mono ? 'break-all font-mono text-xs text-ink' : 'text-sm text-ink'}>
        {value}
      </dd>
    </div>
  );
}

function levelFor(state: NodeState) {
  if (state === 'healthy') return 'healthy' as const;
  if (state === 'maintenance') return 'paused' as const;
  if (state === 'joining' || state === 'draining') return 'pending' as const;
  if (state === 'suspect') return 'warning' as const;
  if (state === 'decommissioned') return 'disabled' as const;
  return 'critical' as const;
}

function capitalise(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
