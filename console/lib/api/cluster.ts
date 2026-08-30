import type {
  ClusterDevice,
  ClusterHealthReport,
  ClusterNode,
  ClusterOperation,
  ClusterStatus,
  RepairStatus,
  StoragePolicy,
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

/** Lists every defined storage class and the policy behind it. */
export function fetchStorageClasses(signal?: AbortSignal): Promise<StoragePolicy[]> {
  return request<StoragePolicy[]>('/v1/storage-classes', signal ? { signal } : {});
}

/** Lists every registered storage device in the cluster. */
export function fetchClusterDevices(signal?: AbortSignal): Promise<ClusterDevice[]> {
  return request<ClusterDevice[]>('/v1/devices', signal ? { signal } : {});
}

/**
 * Device lifecycle actions.
 *
 * Each is its own function with the whole path written out rather than composed
 * from a helper. The repetition is deliberate: the endpoint-coverage test reads
 * these literals to check that every route the console calls is one the server
 * actually serves, and a composed path is invisible to it.
 */

/** Brings a registered device into service. */
export function activateDevice(nodeId: string, deviceId: string): Promise<ClusterDevice> {
  return request<ClusterDevice>(
    `/v1/nodes/${encodeURIComponent(nodeId)}/devices/${encodeURIComponent(deviceId)}/activate`,
    { method: 'POST', body: {} },
  );
}

/** Stops new placement and moves the device's replicas elsewhere. */
export function drainDevice(nodeId: string, deviceId: string): Promise<ClusterDevice> {
  return request<ClusterDevice>(
    `/v1/nodes/${encodeURIComponent(nodeId)}/devices/${encodeURIComponent(deviceId)}/drain`,
    { method: 'POST', body: {} },
  );
}

/** Pauses a device without evacuating it. */
export function maintainDevice(nodeId: string, deviceId: string): Promise<ClusterDevice> {
  return request<ClusterDevice>(
    `/v1/nodes/${encodeURIComponent(nodeId)}/devices/${encodeURIComponent(deviceId)}/maintenance`,
    { method: 'POST', body: {} },
  );
}

/** Returns a drained or paused device to service. */
export function resumeDevice(nodeId: string, deviceId: string): Promise<ClusterDevice> {
  return request<ClusterDevice>(
    `/v1/nodes/${encodeURIComponent(nodeId)}/devices/${encodeURIComponent(deviceId)}/resume`,
    { method: 'POST', body: {} },
  );
}

/**
 * Asks whether a device is safe to remove.
 *
 * The server refuses while the device still owns replicas, which is what makes
 * a success worth trusting. The console never decides this itself.
 */
export function releaseDevice(nodeId: string, deviceId: string): Promise<ClusterDevice> {
  return request<ClusterDevice>(
    `/v1/nodes/${encodeURIComponent(nodeId)}/devices/${encodeURIComponent(deviceId)}/release`,
    { method: 'POST', body: {} },
  );
}

/** Permanently retires a device. */
export function retireDevice(nodeId: string, deviceId: string): Promise<ClusterDevice> {
  return request<ClusterDevice>(
    `/v1/nodes/${encodeURIComponent(nodeId)}/devices/${encodeURIComponent(deviceId)}/retire`,
    { method: 'POST', body: {} },
  );
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
/** Holds every active rebalance without discarding its progress. */
export function pauseRebalance(): Promise<{ operations_changed: number }> {
  return request<{ operations_changed: number }>('/v1/rebalance/pause', {
    method: 'POST',
    body: {},
  });
}

/** Returns paused rebalances to service. */
export function resumeRebalance(): Promise<{ operations_changed: number }> {
  return request<{ operations_changed: number }>('/v1/rebalance/resume', {
    method: 'POST',
    body: {},
  });
}

export function startRebalance(): Promise<ClusterOperation> {
  return request<ClusterOperation>('/v1/rebalance', { method: 'POST' });
}
