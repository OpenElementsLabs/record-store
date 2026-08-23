'use client';

import { useQuery } from '@tanstack/react-query';

import {
  fetchSession,
  fetchStorageStatus,
  fetchStorageUsage,
  fetchSystemInfo,
} from '@/lib/api/system';

/** Query keys, kept in one place so invalidation stays consistent. */
export const queryKeys = {
  systemInfo: ['system', 'info'] as const,
  session: ['auth', 'session'] as const,
  storageUsage: ['storage', 'usage'] as const,
  storageStatus: ['storage', 'status'] as const,
  systemMetrics: ['system', 'metrics'] as const,
  buckets: ['buckets'] as const,
  bucket: (name: string) => ['buckets', name] as const,
  bucketLifecycle: (name: string) => ['buckets', name, 'lifecycle'] as const,
  objects: (bucket: string, prefix: string, cursor: string | null) =>
    ['buckets', bucket, 'objects', prefix, cursor] as const,
  object: (bucket: string, key: string) => ['buckets', bucket, 'object', key] as const,
  objectVersions: (bucket: string, prefix: string) =>
    ['buckets', bucket, 'versions', prefix] as const,
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

export function useStorageStatus() {
  return useQuery({
    queryKey: queryKeys.storageStatus,
    queryFn: ({ signal }) => fetchStorageStatus(signal),
    refetchInterval: 30_000,
  });
}
