'use client';

import { useQuery } from '@tanstack/react-query';
import { CircleCheck, TriangleAlert } from 'lucide-react';
import Link from 'next/link';

import { Card, CardContent } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useClusterEnabled } from '@/features/system/deployment';
import { queryKeys, useStorageStatus } from '@/hooks/use-system';
import { fetchClusterHealth } from '@/lib/api/cluster';
import { fetchWebhookDeliveries } from '@/lib/api/observability';
import { fetchSystemMetrics } from '@/lib/api/system';
import { formatCount, formatRatio } from '@/lib/format';

/** Something an operator should look at, most severe first. */
type Finding = {
  readonly id: string;
  readonly severity: 'warning' | 'critical';
  readonly message: string;
  readonly href: string;
  readonly action: string;
};

/** Utilisation at which a disk warrants attention before it fills. */
const DISK_WARNING = 0.85;

/**
 * Surfaces only what needs attention.
 *
 * Every signal here is a real backend value, and the panel disappears entirely
 * when there is nothing to report — an empty attention list is the normal state
 * and should not occupy the top of the screen with reassurance.
 */
export function AttentionPanel() {
  const clusterEnabled = useClusterEnabled();
  const status = useStorageStatus();
  const metrics = useQuery({
    queryKey: queryKeys.systemMetrics,
    queryFn: ({ signal }) => fetchSystemMetrics(signal),
    refetchInterval: 30_000,
  });
  const deliveries = useQuery({
    queryKey: queryKeys.webhookDeliveries,
    queryFn: ({ signal }) => fetchWebhookDeliveries(undefined, signal),
  });
  const health = useQuery({
    queryKey: queryKeys.clusterHealth,
    queryFn: ({ signal }) => fetchClusterHealth(signal),
    enabled: clusterEnabled,
    refetchInterval: 30_000,
  });

  const loading = status.isPending || metrics.isPending;
  const findings: Finding[] = [];

  if (status.data && status.data.capacity_bytes > 0) {
    const used = status.data.capacity_bytes - status.data.available_bytes;
    const fraction = used / status.data.capacity_bytes;
    if (fraction >= DISK_WARNING) {
      findings.push({
        id: 'disk',
        severity: fraction >= 0.95 ? 'critical' : 'warning',
        message: `Disk is ${formatRatio(used, status.data.capacity_bytes)} full.`,
        href: '/system',
        action: 'View health',
      });
    }
  }

  if (metrics.data && metrics.data.errors > 0 && metrics.data.requests > 0) {
    // A ratio is derivable from two counters; a rate is not, so none is claimed.
    const ratio = metrics.data.errors / metrics.data.requests;
    findings.push({
      id: 'errors',
      severity: ratio >= 0.05 ? 'critical' : 'warning',
      message: `${formatCount(metrics.data.errors)} of ${formatCount(metrics.data.requests)} requests failed since this server started (${formatRatio(metrics.data.errors, metrics.data.requests)}).`,
      href: '/audit',
      action: 'Open audit log',
    });
  }

  const failedDeliveries = (deliveries.data ?? []).filter((entry) => !entry.success).length;
  if (failedDeliveries > 0) {
    findings.push({
      id: 'webhooks',
      severity: 'warning',
      message: `${formatCount(failedDeliveries)} webhook ${failedDeliveries === 1 ? 'delivery has' : 'deliveries have'} failed.`,
      href: '/webhooks',
      action: 'View webhooks',
    });
  }

  const cluster = metrics.data?.cluster;
  if (cluster) {
    if (!cluster.quorum_writable) {
      findings.push({
        id: 'quorum',
        severity: 'critical',
        message: 'Metadata has no writable quorum, so the cluster cannot accept changes.',
        href: '/cluster',
        action: 'View cluster',
      });
    }
    if (cluster.under_replicated_objects > 0) {
      findings.push({
        id: 'under-replicated',
        severity: 'critical',
        message: `${formatCount(cluster.under_replicated_objects)} objects hold fewer replicas than configured.`,
        href: '/cluster',
        action: 'View cluster',
      });
    }
    if (cluster.repair_active_tasks > 0) {
      findings.push({
        id: 'repair',
        severity: 'warning',
        message: `${formatCount(cluster.repair_active_tasks)} repair ${cluster.repair_active_tasks === 1 ? 'task is' : 'tasks are'} running.`,
        href: '/cluster',
        action: 'View cluster',
      });
    }
  }

  for (const reason of health.data?.reasons ?? []) {
    findings.push({
      id: `health-${reason}`,
      severity: 'warning',
      message: reason,
      href: '/cluster',
      action: 'View cluster',
    });
  }

  if (loading) {
    return (
      <Card>
        <CardContent>
          <Skeleton className="h-10 w-full" />
        </CardContent>
      </Card>
    );
  }

  if (findings.length === 0) {
    return (
      <Card>
        <CardContent className="flex items-center gap-2 py-3">
          <CircleCheck aria-hidden className="size-4 shrink-0 text-ok" />
          <p className="text-sm text-ink-muted">Nothing needs attention.</p>
        </CardContent>
      </Card>
    );
  }

  const ordered = [...findings].sort((left, right) =>
    left.severity === right.severity ? 0 : left.severity === 'critical' ? -1 : 1,
  );

  return (
    <Card>
      <ul className="divide-y divide-border">
        {ordered.map((finding) => (
          <li key={finding.id} className="flex flex-wrap items-center gap-x-3 gap-y-1.5 px-4 py-3">
            <TriangleAlert
              aria-hidden
              className={`size-4 shrink-0 ${finding.severity === 'critical' ? 'text-danger' : 'text-warn'}`}
            />
            <span className="sr-only">
              {finding.severity === 'critical' ? 'Critical: ' : 'Warning: '}
            </span>
            <p className="min-w-0 flex-1 type-body">{finding.message}</p>
            <Link
              href={finding.href}
              className="shrink-0 text-xs font-medium text-accent hover:underline"
            >
              {finding.action}
            </Link>
          </li>
        ))}
      </ul>
    </Card>
  );
}
