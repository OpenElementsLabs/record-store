'use client';

import { useQuery } from '@tanstack/react-query';

import {
  fetchReadiness,
  fetchSession,
  fetchStorageStatus,
  fetchStorageUsage,
  fetchSystemInfo,
} from '@/lib/api/system';

/** Query keys, kept in one place so invalidation stays consistent. */
export const queryKeys = {
  systemInfo: ['system', 'info'] as const,
  readiness: ['system', 'readiness'] as const,
  session: ['auth', 'session'] as const,
  storageUsage: ['storage', 'usage'] as const,
  storageStatus: ['storage', 'status'] as const,
  systemMetrics: ['system', 'metrics'] as const,
  // A sibling of systemMetrics rather than a child: react-query matches keys by
  // prefix, so nesting it would make every refetch of the counters also refetch
  // the history it is only ever seeded from once.
  systemMetricsHistory: ['system', 'metrics-history'] as const,
  buckets: ['buckets'] as const,
  bucket: (name: string) => ['buckets', name] as const,
  bucketLifecycle: (name: string) => ['buckets', name, 'lifecycle'] as const,
  objects: (bucket: string, prefix: string, cursor: string | null) =>
    ['buckets', bucket, 'objects', prefix, cursor] as const,
  object: (bucket: string, key: string) => ['buckets', bucket, 'object', key] as const,
  objectVersions: (bucket: string, prefix: string) =>
    ['buckets', bucket, 'versions', prefix] as const,
  // Capability keys sit under the object they belong to, so invalidating after
  // creating a share touches that object's lists and nothing else.
  objectShares: (bucket: string, key: string) =>
    ['buckets', bucket, 'object', key, 'shares'] as const,
  objectEmbeds: (bucket: string, key: string) =>
    ['buckets', bucket, 'object', key, 'embeds'] as const,
  share: (id: string) => ['shares', id] as const,
  shareUrl: (id: string) => ['shares', id, 'url'] as const,
  embed: (id: string) => ['embeds', id] as const,
  embedUrl: (id: string) => ['embeds', id, 'url'] as const,
  sharingSettings: ['sharing', 'settings'] as const,
  serviceAccounts: ['service-accounts'] as const,
  serviceAccount: (id: string) => ['service-accounts', id] as const,
  policies: ['policies'] as const,
  audit: (key: string) => ['audit', key] as const,
  events: (key: string) => ['events', key] as const,
  webhooks: ['webhooks'] as const,
  webhookDeliveries: ['webhook-deliveries'] as const,
  storageInspection: (limit: number) => ['storage', 'inspection', limit] as const,
  clusterStatus: ['cluster', 'status'] as const,
  clusterHealth: ['cluster', 'health'] as const,
  clusterNodes: ['cluster', 'nodes'] as const,
  clusterDevices: ['cluster', 'devices'] as const,
  storageClasses: ['cluster', 'storage-classes'] as const,
  clusterRepair: ['cluster', 'repair'] as const,
  clusterRebalance: ['cluster', 'rebalance'] as const,
  clusterNode: (id: string) => ['cluster', 'nodes', id] as const,
};

export function useSystemInfo() {
  return useQuery({
    queryKey: queryKeys.systemInfo,
    queryFn: ({ signal }) => fetchSystemInfo(signal),
    staleTime: 60_000,
  });
}

export function useSession() {
  return useQuery({
    queryKey: queryKeys.session,
    queryFn: ({ signal }) => fetchSession(signal),
    staleTime: 60_000,
  });
}

/** Storage usage refreshes on a modest interval; it changes with traffic. */
export function useStorageUsage() {
  return useQuery({
    queryKey: queryKeys.storageUsage,
    queryFn: ({ signal }) => fetchStorageUsage(signal),
    refetchInterval: 15_000,
  });
}

/**
 * Polls the server's readiness probe.
 *
 * Deliberately never retried and never treated as an error: every outcome is a
 * state the screen renders, and a retry would only delay showing the operator
 * that the server is refusing to serve.
 */
export function useReadiness() {
  return useQuery({
    queryKey: queryKeys.readiness,
    queryFn: ({ signal }) => fetchReadiness(signal),
    refetchInterval: 30_000,
    retry: false,
  });
}

export function useStorageStatus() {
  return useQuery({
    queryKey: queryKeys.storageStatus,
    queryFn: ({ signal }) => fetchStorageStatus(signal),
    refetchInterval: 30_000,
  });
}
