import { screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { DurabilityScreen } from './durability-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { ClusterStatus, RepairStatus } from '@/types/cluster';

function status(overrides: Partial<ClusterStatus['replication']> = {}): ClusterStatus {
  return {
    cluster_id: 'c1',
    health: 'healthy',
    metadata: {
      status: { health: 'healthy', members: 3, healthy_members: 3, writable: true },
      leader: 'node-1',
    },
    data: { health: 'healthy', reasons: [] },
    replication: {
      replication_factor: 3,
      required_acknowledgements: 2,
      payloads: 1_000,
      logical_bytes: 1_000_000_000,
      physical_bytes: 3_000_000_000,
      under_replicated_payloads: 0,
      unavailable_payloads: 0,
      tombstones: 0,
      ...overrides,
    },
    repair: { active_tasks: 0, parked_tasks: 0 },
    nodes: [],
    operations: [],
    local_tasks: {},
    observed_at: '2026-08-23T10:00:00Z',
  } as unknown as ClusterStatus;
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(cluster: ClusterStatus, repair: RepairStatus) {
  fetchMock.mockImplementation((url: string) =>
    Promise.resolve(jsonResponse(String(url).includes('/repair/status') ? repair : cluster)),
  );
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('DurabilityScreen', () => {
  it('states the configured durability in plain terms', async () => {
    respond(status(), { active_tasks: 0, parked_tasks: 0 });
    renderWithProviders(<DurabilityScreen />);

    expect(
      await screen.findByText('3 replicas, 2 acknowledged before a write succeeds'),
    ).toBeTruthy();
  });

  it('reports amplification as a ratio of physical to logical', async () => {
    respond(status(), { active_tasks: 0, parked_tasks: 0 });
    renderWithProviders(<DurabilityScreen />);

    // 3 GB stored for 1 GB of data is the number that matters, not either alone.
    expect(await screen.findByText('300%')).toBeTruthy();
  });

  it('reads the repair queue from its own endpoint', async () => {
    respond(status(), { active_tasks: 4, parked_tasks: 2 });
    renderWithProviders(<DurabilityScreen />);

    expect(await screen.findByText('Tasks running')).toBeTruthy();
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/v1/repair/status'))).toBe(
      true,
    );
  });

  it('warns that parked repair tasks will not retry themselves', async () => {
    respond(status(), { active_tasks: 0, parked_tasks: 3 });
    renderWithProviders(<DurabilityScreen />);

    expect(await screen.findByText(/will not retry on their own/)).toBeTruthy();
  });

  it('says repair cannot rebuild payloads with no readable copy', async () => {
    respond(status({ unavailable_payloads: 5 }), { active_tasks: 0, parked_tasks: 0 });
    renderWithProviders(<DurabilityScreen />);

    // Redundancy is what repair reads from; with none left it cannot help, and
    // implying otherwise would be the dangerous kind of wrong.
    expect(await screen.findByText(/there is none left to read/)).toBeTruthy();
  });

  it('does not claim throughput the backend never reports', async () => {
    respond(status(), { active_tasks: 2, parked_tasks: 0 });
    renderWithProviders(<DurabilityScreen />);
    await screen.findByText('Tasks running');

    expect(screen.getByText(/Per-job detail and throughput are not exposed/)).toBeTruthy();
    expect(document.body.textContent ?? '').not.toMatch(/MB\/s|bytes\/s|per second/i);
  });
});
