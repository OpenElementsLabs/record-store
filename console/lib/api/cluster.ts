import type {
  ClusterHealthReport,
  ClusterNode,
  ClusterOperation,
  ClusterStatus,
  RepairStatus,
} from '@/types/cluster';

import { request } from './client';

/** Reads the whole cluster status document. */
export function fetchClusterStatus(signal?: AbortSignal): Promise<ClusterStatus> {
  return request<ClusterStatus>('/v1/cluster', signal ? { signal } : {});
}

/** Reads the richer health report, with reasons an operator can act on. */
export function fetchClusterHealth(signal?: AbortSignal): Promise<ClusterHealthReport> {
  return request<ClusterHealthReport>('/v1/cluster/health', signal ? { signal } : {});
}

export function fetchClusterNodes(signal?: AbortSignal): Promise<ClusterNode[]> {
  return request<ClusterNode[]>('/v1/nodes', signal ? { signal } : {});
}

export function fetchClusterNode(id: string, signal?: AbortSignal): Promise<ClusterNode> {
  return request<ClusterNode>(`/v1/nodes/${encodeURIComponent(id)}`, signal ? { signal } : {});
}

/** Stops new placement on a node and moves its replicas elsewhere. */
export function drainNode(id: string): Promise<ClusterOperation> {
  return request<ClusterOperation>(`/v1/nodes/${encodeURIComponent(id)}/drain`, {
    method: 'POST',
    body: {},
  });
}

/** Pauses a node without moving its data. */
export function maintainNode(id: string): Promise<void> {
  return request<void>(`/v1/nodes/${encodeURIComponent(id)}/maintenance`, {
    method: 'POST',
    body: {},
  });
}

export function resumeNode(id: string): Promise<void> {
  return request<void>(`/v1/nodes/${encodeURIComponent(id)}/resume`, {
    method: 'POST',
    body: {},
  });
}

/**
 * Permanently removes a node.
 *
 * The backend refuses this when it would drop object versions below their
 * required durability unless `force` is set, and the console surfaces that
 * refusal rather than retrying with force on the user's behalf.
 */
export function decommissionNode(id: string, force: boolean): Promise<ClusterOperation> {
  return request<ClusterOperation>(`/v1/nodes/${encodeURIComponent(id)}/decommission`, {
    method: 'POST',
    body: { force },
  });
}

/**
 * Reads the repair queue.
 *
 * Narrower than the full cluster document, so a screen that only watches repair
 * does not pull every node and operation on each refresh.
 */
export function fetchRepairStatus(signal?: AbortSignal): Promise<RepairStatus> {
  return request<RepairStatus>('/v1/repair/status', signal ? { signal } : {});
}

/** Reads rebalance operations, past and present. */
export function fetchRebalanceOperations(signal?: AbortSignal): Promise<ClusterOperation[]> {
  return request<ClusterOperation[]>('/v1/rebalance/status', signal ? { signal } : {});
}

/**
 * Asks the cluster to rebalance.
 *
 * The backend decides whether a rebalance is warranted and how much movement it
 * will allow; this only requests one.
 */
export function startRebalance(): Promise<ClusterOperation> {
  return request<ClusterOperation>('/v1/rebalance', { method: 'POST' });
}
