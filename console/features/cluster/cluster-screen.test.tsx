import { screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ClusterScreen } from './cluster-screen';
import { jsonResponse, renderWithProviders, systemInfo } from '@/test/render';
import type { ClusterStatus } from '@/types/cluster';

function status(overrides: Partial<ClusterStatus> = {}): ClusterStatus {
  return {
    cluster_id: 'cluster-1',
    health: 'healthy',
    metadata: {
      status: {
        members: 3,
        healthy_members: 3,
        quorum: 2,
        leader: 'node-1',
        writable: true,
        readable: true,
        health: 'healthy',
        fault_tolerant: true,
        notes: [],
      },
      role: 'leader',
      member_id: 1,
      applied_index: 42,
      last_log_index: 42,
      snapshot_index: 30,
      members: [],
    },
    data: {
      nodes: 3,
      healthy_nodes: 3,
      unavailable_nodes: 0,
      under_replicated_payloads: 0,
      unavailable_payloads: 0,
      writable: true,
      health: 'healthy',
      notes: [],
    },
    replication: {
      replication_factor: 3,
      required_acknowledgements: 2,
      payloads: 8,
      logical_bytes: 1_024,
      physical_bytes: 3_072,
      under_replicated_payloads: 0,
      unavailable_payloads: 0,
      tombstones: 0,
    },
    repair: { active_tasks: 0, parked_tasks: 0 },
    nodes: [],
    operations: [],
    local_tasks: {},
    observed_at: '2026-08-22T12:00:00Z',
    ...overrides,
  };
}

afterEach(() => vi.unstubAllGlobals());

describe('ClusterScreen', () => {
  it('separates healthy data and metadata dimensions', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(status())));
    renderWithProviders(<ClusterScreen />, {
      info: systemInfo({
        mode: 'cluster',
        capabilities: { ...systemInfo().capabilities, cluster: true },
      }),
    });

    expect(await screen.findByText('3 of 3 members')).toBeTruthy();
    expect(screen.getByText('Accepted')).toBeTruthy();
    expect(screen.getByText('3 healthy')).toBeTruthy();
    expect(screen.getByText('Yes')).toBeTruthy();
  });

  it('shows actionable degradation reasons instead of hiding them', async () => {
    const degraded = status({
      health: 'degraded',
      data: {
        ...status().data,
        health: 'degraded',
        healthy_nodes: 2,
        under_replicated_payloads: 4,
        notes: ['four payloads need another replica'],
      },
      replication: { ...status().replication, under_replicated_payloads: 4 },
    });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(degraded)));
    renderWithProviders(<ClusterScreen />);

    expect(await screen.findByText('What needs attention')).toBeTruthy();
    expect(screen.getByText('four payloads need another replica')).toBeTruthy();
    expect(screen.getByText('4')).toBeTruthy();
  });
});
