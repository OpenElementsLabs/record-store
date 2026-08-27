import { screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { NodeDetails } from './node-details';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { ClusterNode } from '@/types/cluster';

const node: ClusterNode = {
  node_id: '018f-node-identifier',
  member_id: 7,
  state: 'healthy',
  metadata_voter: true,
  rpc_address: 'storage-2.internal:7603',
  storage_class: 'nvme',
  failure_domain: { region: 'east', zone: 'z2', rack: 'r7' },
  software_version: '0.1.0',
  capacity_bytes: 10_000,
  available_bytes: 7_500,
  utilization_percent: 25,
  replicas: 14,
  last_heartbeat_at: '2026-08-22T12:00:00Z',
  state_changed_at: '2026-08-22T11:00:00Z',
  state_reason: null,
};

afterEach(() => vi.unstubAllGlobals());

describe('NodeDetails', () => {
  it('loads the requested node and presents identity, topology, and capacity', async () => {
    const fetch = vi.fn().mockResolvedValue(jsonResponse(node));
    vi.stubGlobal('fetch', fetch);
    renderWithProviders(<NodeDetails nodeId={node.node_id} />);

    expect(await screen.findByText(node.node_id)).toBeTruthy();
    expect(screen.getByText('storage-2.internal:7603')).toBeTruthy();
    expect(screen.getByText('Voting member')).toBeTruthy();
    expect(screen.getByText('region=east')).toBeTruthy();
    expect(screen.getByText('zone=z2')).toBeTruthy();
    expect(screen.getByText('2.50 kB used of 10.0 kB')).toBeTruthy();
    expect(String(fetch.mock.calls[0]?.[0])).toContain(
      '/api/record-store/v1/nodes/018f-node-identifier',
    );
  });
});
