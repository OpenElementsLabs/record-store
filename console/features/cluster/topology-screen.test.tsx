import { screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { TopologyScreen } from './topology-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { ClusterDevice, ClusterNode } from '@/types/cluster';

function device(overrides: Partial<ClusterDevice> = {}): ClusterDevice {
  return {
    device_id: `device-${Math.random().toString(16).slice(2)}`,
    node_id: 'node-1',
    kind: 'nvme',
    storage_class: 'standard',
    state: 'active',
    health: 'healthy',
    capacity_bytes: 1_000,
    usable_bytes: 1_000,
    available_bytes: 400,
    utilization_percent: 60,
    configured_weight: 1_000,
    accepts_placement: true,
    current_path: null,
    model: null,
    ...overrides,
  };
}

function node(id: string, labels: Record<string, string>, devices: ClusterDevice[]): ClusterNode {
  return {
    node_id: id,
    member_id: 1,
    state: 'healthy',
    metadata_voter: true,
    rpc_address: '127.0.0.1:7603',
    storage_class: 'standard',
    failure_domain: labels,
    software_version: 'test',
    capacity_bytes: 1_000,
    available_bytes: 400,
    utilization_percent: 60,
    replicas: 0,
    last_heartbeat_at: '2026-08-30T12:00:00Z',
    state_changed_at: '2026-08-30T12:00:00Z',
    state_reason: null,
    devices,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

function respond(nodes: ClusterNode[]) {
  fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(nodes)));
}

describe('TopologyScreen', () => {
  it('draws only the levels the deployment actually labels', async () => {
    // Racks, but no regions or datacenters. Rendering empty containers for the
    // levels nobody labelled would imply a hierarchy that does not exist.
    respond([node('node-a', { rack: 'a' }, [device()]), node('node-b', { rack: 'b' }, [device()])]);
    renderWithProviders(<TopologyScreen />);

    expect(await screen.findByText('rack a')).toBeTruthy();
    expect(screen.getByText('rack b')).toBeTruthy();
    expect(screen.queryByText(/region/)).toBeNull();
    expect(screen.queryByText(/datacenter/)).toBeNull();
  });

  it('nests deeper levels inside shallower ones', async () => {
    respond([
      node('node-a', { region: 'ug-central', rack: 'a' }, [device()]),
      node('node-b', { region: 'ug-central', rack: 'b' }, [device()]),
    ]);
    renderWithProviders(<TopologyScreen />);

    expect(await screen.findByText('region ug-central')).toBeTruthy();
    expect(screen.getByText('rack a')).toBeTruthy();
    expect(screen.getByText('rack b')).toBeTruthy();
  });

  it('says an unlabelled group is not proven separate', async () => {
    // Placement treats unlabelled nodes as one domain. Drawing them as their own
    // rack would show separation the cluster cannot actually guarantee.
    respond([node('node-a', { rack: 'a' }, [device()]), node('node-b', {}, [device()])]);
    renderWithProviders(<TopologyScreen />);

    expect(await screen.findByText('No rack')).toBeTruthy();
    expect(screen.getByText('not proven separate')).toBeTruthy();
  });

  it('explains an entirely unlabelled cluster rather than showing nothing', async () => {
    respond([node('node-a', {}, [device()])]);
    renderWithProviders(<TopologyScreen />);

    expect(await screen.findByText(/No topology labels are configured/)).toBeTruthy();
  });

  it('shows the devices under each node', async () => {
    respond([
      node('node-a', { rack: 'a' }, [
        device({ device_id: 'aaaaaaaa-0000-4000-8000-000000000001' }),
        device({
          device_id: 'bbbbbbbb-0000-4000-8000-000000000002',
          state: 'draining',
          accepts_placement: false,
        }),
      ]),
    ]);
    renderWithProviders(<TopologyScreen />);

    expect(await screen.findByTitle('aaaaaaaa-0000-4000-8000-000000000001')).toBeTruthy();
    expect(screen.getByTitle('bbbbbbbb-0000-4000-8000-000000000002')).toBeTruthy();
    // A device taking no new data is called out where it sits in the topology.
    expect(screen.getByText('draining')).toBeTruthy();
  });
});
