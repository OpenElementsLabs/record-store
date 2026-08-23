import { screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { HealthScreen } from './health-screen';
import { jsonResponse, renderWithProviders, systemInfo } from '@/test/render';

let fetchMock: ReturnType<typeof vi.fn>;

function respond(options: { clusterWritable?: boolean; dataHealth?: string } = {}) {
  fetchMock.mockImplementation((url: string) => {
    const target = String(url);
    if (target.includes('/cluster/health')) {
      return Promise.resolve(
        jsonResponse({
          health: 'healthy',
          metadata: {
            status: {
              members: 3,
              healthy_members: 3,
              quorum: 2,
              leader: 'n1',
              writable: options.clusterWritable ?? true,
              readable: true,
              health: 'healthy',
              fault_tolerant: true,
              notes: [],
            },
          },
          data: { health: options.dataHealth ?? 'healthy', reasons: [] },
          reasons: [],
        }),
      );
    }
    if (target.includes('/storage/status')) {
      return Promise.resolve(
        jsonResponse({
          capacity_bytes: 1_000_000_000,
          available_bytes: 800_000_000,
          temporary_upload_bytes: 0,
        }),
      );
    }
    return Promise.resolve(
      jsonResponse({
        object_count: 1,
        bytes_used: 10,
        bucket_count: 1,
        version_count: 1,
        version_bytes: 10,
        physical_bytes: 10,
        temporary_multipart_bytes: 0,
      }),
    );
  });
}

const clustered = systemInfo({
  mode: 'cluster',
  capabilities: { ...systemInfo().capabilities, cluster: true },
});

beforeEach(() => {
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => vi.unstubAllGlobals());

describe('HealthScreen subsystems', () => {
  it('reports cluster components as not enabled in standalone, never as failing', async () => {
    respond();
    renderWithProviders(<HealthScreen />);

    // Showing "critical" for a component this deployment does not run would
    // train operators to ignore the screen.
    expect(await screen.findByText('Subsystems')).toBeTruthy();
    expect(screen.getAllByText('Not enabled')).toHaveLength(2);
    expect(screen.getByText(/standalone server keeps a single copy/)).toBeTruthy();
    expect(screen.queryByText('No quorum')).toBeNull();
  });

  it('always reports the parts every deployment runs', async () => {
    respond();
    renderWithProviders(<HealthScreen />);

    expect(await screen.findByText('Management API')).toBeTruthy();
    expect(screen.getByText('Object storage')).toBeTruthy();
    // 'Metadata' also appears in the summary strip at the top of the screen.
    expect(screen.getAllByText('Metadata').length).toBeGreaterThan(0);
  });

  it('reports real consensus and replication state in a cluster', async () => {
    respond();
    renderWithProviders(<HealthScreen />, { info: clustered });

    expect(await screen.findByText('Writable')).toBeTruthy();
    expect(screen.queryByText('Not enabled')).toBeNull();
  });

  it('surfaces a lost quorum as critical rather than as disabled', async () => {
    respond({ clusterWritable: false });
    renderWithProviders(<HealthScreen />, { info: clustered });

    expect(await screen.findByText('No quorum')).toBeTruthy();
  });

  it('carries degraded replication through from the backend', async () => {
    respond({ dataHealth: 'degraded' });
    renderWithProviders(<HealthScreen />, { info: clustered });

    expect(await screen.findByText('Degraded')).toBeTruthy();
  });
});
