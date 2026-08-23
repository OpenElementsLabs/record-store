import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { RebalanceScreen } from './rebalance-screen';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { ClusterOperation, ClusterOperationState } from '@/types/cluster';

function operation(
  state: ClusterOperationState,
  progress: Partial<ClusterOperation['progress']> = {},
  id = 'op-1',
): ClusterOperation {
  return {
    id,
    kind: 'rebalance',
    node_id: null,
    state,
    progress: {
      objects_remaining: 0,
      bytes_remaining: 0,
      objects_moved: 0,
      bytes_moved: 0,
      replicas_moving: 0,
      tasks_parked: 0,
      ...progress,
    },
    started_at: '2026-08-23T10:00:00Z',
    updated_at: '2026-08-23T10:05:00Z',
    completed_at: state === 'completed' ? '2026-08-23T10:10:00Z' : null,
    message: null,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(operations: ClusterOperation[]) {
  fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(operations)));
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('RebalanceScreen', () => {
  it('says plainly that rebalancing does not change replica count', async () => {
    respond([]);
    renderWithProviders(<RebalanceScreen />);

    expect(await screen.findByText(/never changes how many copies exist/)).toBeTruthy();
  });

  it('reports progress against a known total', async () => {
    respond([
      operation('moving', {
        bytes_moved: 250_000_000,
        bytes_remaining: 750_000_000,
        objects_moved: 40,
        objects_remaining: 120,
      }),
    ]);
    renderWithProviders(<RebalanceScreen />);

    const bar = await screen.findByRole('progressbar', { name: /Rebalance progress/ });
    expect(bar.getAttribute('aria-valuenow')).toBe('25');
    expect(screen.getByText(/250 MB of 1\.00 GB moved/)).toBeTruthy();
  });

  it('does not invent a percentage while the work is still being counted', async () => {
    respond([operation('planning')]);
    renderWithProviders(<RebalanceScreen />);

    // Nothing has been counted yet, so a 0% bar would misrepresent the state.
    expect(await screen.findByText(/has not been counted yet/)).toBeTruthy();
    expect(screen.queryByRole('progressbar')).toBeNull();
  });

  it('surfaces parked transfers rather than hiding them in a total', async () => {
    respond([operation('moving', { bytes_moved: 10, bytes_remaining: 90, tasks_parked: 4 })]);
    renderWithProviders(<RebalanceScreen />);

    expect(await screen.findByText(/4 transfers parked/)).toBeTruthy();
  });

  it('separates finished operations from running ones', async () => {
    respond([
      operation('completed', { bytes_moved: 100, objects_moved: 5 }, 'op-done'),
      operation('moving', { bytes_moved: 10, bytes_remaining: 90 }, 'op-live'),
    ]);
    renderWithProviders(<RebalanceScreen />);

    expect(await screen.findByText('In progress')).toBeTruthy();
    expect(screen.getByText('History')).toBeTruthy();
    expect(screen.getByText('Completed')).toBeTruthy();
  });

  it('will not start a second rebalance while one is running', async () => {
    respond([operation('moving', { bytes_moved: 10, bytes_remaining: 90 })]);
    renderWithProviders(<RebalanceScreen />);

    // Wait for the running operation to load: the header renders before the
    // query resolves, so asserting immediately would test the empty state.
    await screen.findByText('In progress');
    expect(screen.getByRole('button', { name: 'Start rebalance' }).hasAttribute('disabled')).toBe(
      true,
    );
  });

  it('confirms before requesting a rebalance and explains what it does', async () => {
    respond([]);
    renderWithProviders(<RebalanceScreen />);
    await screen.findByText('No rebalance is running.');

    await userEvent.click(screen.getByRole('button', { name: 'Start rebalance' }));
    const dialog = await screen.findByRole('dialog');
    expect(dialog.textContent).toMatch(/does not change how many copies/);
    expect(fetchMock.mock.calls.some(([, init]) => init?.method === 'POST')).toBe(false);

    await userEvent.click(within(dialog).getByRole('button', { name: 'Start rebalance' }));
    await waitFor(() => {
      const call = fetchMock.mock.calls.find(([, init]) => init?.method === 'POST');
      expect(String(call?.[0])).toContain('/v1/rebalance');
    });
  });

  it('offers no rebalance control to a role that cannot operate the cluster', async () => {
    respond([]);
    renderWithProviders(<RebalanceScreen />, { session: session(auditorPermissions) });
    await screen.findByText('No rebalance is running.');

    expect(screen.queryByRole('button', { name: 'Start rebalance' })).toBeNull();
  });
});
