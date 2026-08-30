import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { DrivesScreen } from './drives-screen';
import { auditorPermissions, jsonResponse, renderWithProviders, session } from '@/test/render';
import type { ClusterDevice } from '@/types/cluster';

function device(overrides: Partial<ClusterDevice> = {}): ClusterDevice {
  return {
    device_id: '11111111-1111-4111-8111-111111111111',
    node_id: '22222222-2222-4222-8222-222222222222',
    kind: 'nvme',
    storage_class: 'standard',
    state: 'active',
    health: 'healthy',
    capacity_bytes: 1_000_000_000,
    usable_bytes: 1_000_000_000,
    available_bytes: 400_000_000,
    utilization_percent: 60,
    configured_weight: 1_000,
    accepts_placement: true,
    current_path: '/srv/record-store/disk-1',
    model: null,
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

function respond(devices: ClusterDevice[]) {
  fetchMock.mockImplementation(() => Promise.resolve(jsonResponse(devices)));
}

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('DrivesScreen', () => {
  it('reports lifecycle and health as separate facts', async () => {
    // A drive an administrator left active, whose platform reports it failed.
    // Collapsing the two would hide exactly the situation an operator needs.
    respond([device({ state: 'active', health: 'failed', accepts_placement: false })]);
    renderWithProviders(<DrivesScreen />);

    expect(await screen.findByText('Active')).toBeTruthy();
    expect(screen.getByText('Failed')).toBeTruthy();
    expect(screen.getByText('no new data')).toBeTruthy();
  });

  it('does not present unknown health as healthy', async () => {
    respond([device({ health: 'unknown' })]);
    renderWithProviders(<DrivesScreen />);

    // The platform not reporting health is not the same as reporting good
    // health, and showing it as healthy would invite misplaced trust.
    const badge = await screen.findByText('Unknown');
    expect(badge).toBeTruthy();
    expect(screen.queryByText('Healthy')).toBeNull();
  });

  it('addresses a device by its stable identifier, not its path', async () => {
    respond([device({ current_path: '/dev/sda' })]);
    renderWithProviders(<DrivesScreen />);

    expect(await screen.findByTitle('11111111-1111-4111-8111-111111111111')).toBeTruthy();
    // The path is shown, but only as description.
    expect(screen.getByText('/dev/sda')).toBeTruthy();
  });

  it('says a path is unknown rather than leaving a blank cell', async () => {
    respond([device({ current_path: null })]);
    renderWithProviders(<DrivesScreen />);

    expect(await screen.findByText('path unknown')).toBeTruthy();
  });

  it('warns that retiring a device cannot be undone', async () => {
    respond([device()]);
    renderWithProviders(<DrivesScreen />);

    await userEvent.click(
      await screen.findByRole('button', {
        name: /Actions for device 11111111-1111-4111-8111-111111111111/,
      }),
    );
    await userEvent.click(await screen.findByText('Retire device'));

    expect(
      await screen.findByText(/Retire only a device that has already been drained/),
    ).toBeTruthy();
  });

  it('describes the safe-to-remove check as a question the server answers', async () => {
    respond([device()]);
    renderWithProviders(<DrivesScreen />);

    await userEvent.click(
      await screen.findByRole('button', {
        name: /Actions for device 11111111-1111-4111-8111-111111111111/,
      }),
    );
    await userEvent.click(await screen.findByText('Check if safe to remove'));

    // The console must not decide this itself; only the server knows whether the
    // device still owns replicas.
    expect(await screen.findByText(/refused while the device still owns replicas/)).toBeTruthy();
  });

  it('offers no actions to a role that may not operate the cluster', async () => {
    respond([device()]);
    renderWithProviders(<DrivesScreen />, {
      session: session(auditorPermissions),
    });

    await waitFor(() => expect(screen.getByText('Active')).toBeTruthy());
    expect(screen.queryByRole('button', { name: /Actions for device/ })).toBeNull();
  });

  it('explains that discovering a disk does not enrol it', async () => {
    respond([]);
    renderWithProviders(<DrivesScreen />);

    expect(await screen.findByText(/Discovering a disk never enrolls it/)).toBeTruthy();
  });
});
