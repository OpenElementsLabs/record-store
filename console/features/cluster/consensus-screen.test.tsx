import { screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ConsensusScreen } from './consensus-screen';
import { jsonResponse, renderWithProviders } from '@/test/render';
import type { ClusterStatus, MetadataQuorum } from '@/types/cluster';

function quorum(overrides: Partial<MetadataQuorum['status']> = {}): ClusterStatus {
  return {
    cluster_id: 'c1',
    health: 'healthy',
    metadata: {
      status: {
        members: 3,
        healthy_members: 3,
        quorum: 2,
        leader: 'node-aaaaaaaa-bbbb',
        writable: true,
        readable: true,
        health: 'healthy',
        fault_tolerant: true,
        notes: [],
        ...overrides,
      },
      role: 'leader',
      member_id: 1,
      applied_index: 4_200,
      last_log_index: 4_200,
      snapshot_index: 4_000,
      members: [
        { member_id: 1, address: '10.0.0.1:7603', voter: true, reachable: true },
        { member_id: 2, address: '10.0.0.2:7603', voter: true, reachable: true },
        { member_id: 3, address: '10.0.0.3:7603', voter: false, reachable: false },
      ],
    },
    data: { health: 'healthy', reasons: [] },
    replication: {},
    repair: { active_tasks: 0, parked_tasks: 0 },
    nodes: [],
    operations: [],
    local_tasks: {},
    observed_at: '2026-08-23T12:00:00Z',
  } as unknown as ClusterStatus;
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('ConsensusScreen', () => {
  it('answers whether metadata changes are being accepted, in words', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(quorum())));
    renderWithProviders(<ConsensusScreen />);

    expect(await screen.findByText('Metadata changes are being accepted.')).toBeTruthy();
    expect(screen.getByText('3 of 3, needs 2')).toBeTruthy();
  });

  it('warns when the group can no longer lose a member', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse(quorum({ fault_tolerant: false, healthy_members: 2 }))),
    );
    renderWithProviders(<ConsensusScreen />);

    // This is the state an operator must act on before the next failure, so it
    // is stated rather than left to be inferred from member counts.
    expect(
      await screen.findByText('Losing one more member would stop metadata changes.'),
    ).toBeTruthy();
  });

  it('says metadata is refused when there is no writable majority', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        jsonResponse(
          quorum({ writable: false, readable: true, health: 'critical', healthy_members: 1 }),
        ),
      ),
    );
    renderWithProviders(<ConsensusScreen />);

    expect(await screen.findByText(/no writable majority/)).toBeTruthy();
    expect(screen.getByText('Read-only')).toBeTruthy();
  });

  it('distinguishes a voting member from one that cannot help reach agreement', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(quorum())));
    renderWithProviders(<ConsensusScreen />);

    expect(await screen.findByText('Non-voting')).toBeTruthy();
    expect(screen.getAllByText('Voter')).toHaveLength(2);
    expect(screen.getByText('Unreachable')).toBeTruthy();
  });

  it('marks which member is this node', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(quorum())));
    renderWithProviders(<ConsensusScreen />);

    expect(await screen.findByText('this node')).toBeTruthy();
  });

  it('says a position is unestablished rather than showing zero', async () => {
    fetchMock.mockImplementation(() => {
      const status = quorum();
      return Promise.resolve(
        jsonResponse({
          ...status,
          metadata: { ...status.metadata, snapshot_index: null },
        }),
      );
    });
    renderWithProviders(<ConsensusScreen />);

    // A missing snapshot is not a snapshot at position zero.
    expect(await screen.findByText('not yet established')).toBeTruthy();
  });

  it('reports no elected leader plainly', async () => {
    fetchMock.mockImplementation(() =>
      Promise.resolve(jsonResponse(quorum({ leader: null, writable: false }))),
    );
    renderWithProviders(<ConsensusScreen />);

    expect(await screen.findByText('none elected')).toBeTruthy();
  });
});
