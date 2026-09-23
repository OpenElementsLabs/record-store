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
    if (target.includes('/api/readiness')) {
      return Promise.resolve(jsonResponse({ state: 'ready' }));
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

it('never calls pending observations healthy', () => {
  fetchMock.mockImplementation(() => new Promise(() => {}));
  renderWithProviders(<HealthScreen />);
  expect(screen.queryByText('Responding')).toBeNull();
  expect(screen.queryByText('Ready')).toBeNull();
});

it('shows unavailable usage rather than leaving an endless loading placeholder', async () => {
  // A fresh Response per call: a body can only be read once, so returning one
  // shared instance makes whichever query reads first the only one that sees it.
  fetchMock.mockImplementation(() =>
    Promise.resolve(
      jsonResponse(
        {
          error: {
            code: 'UNAVAILABLE',
            message: 'Observation unavailable',
            request_id: 'req-health',
          },
        },
        503,
      ),
    ),
  );
  renderWithProviders(<HealthScreen />);
  expect((await screen.findAllByText('Observation unavailable')).length).toBeGreaterThan(0);
  expect(screen.queryByText('Responding')).toBeNull();
});

describe('service readiness', () => {
  /**
   * Liveness and readiness answer different questions, and an operator needs
   * them apart. A process that responds to every request while its storage is
   * read-only is live and not ready; one "Responding" badge would call both
   * healthy.
   */
  it('reports a confirmed write probe as ready, and says what that established', async () => {
    respond();
    renderWithProviders(<HealthScreen />);

    expect(await screen.findByText('Ready')).toBeTruthy();
    expect(screen.getByText(/write, synchronise, and delete probe/)).toBeTruthy();
    // Readiness must not be allowed to imply anything about stored objects.
    expect(
      screen.getByText(/says nothing about the integrity of objects already stored/),
    ).toBeTruthy();
  });

  /**
   * A server that answers and declines to serve is a working process with a
   * storage problem. Reporting it as unreachable would send an operator to the
   * network; reporting it as healthy would hide an outage.
   */
  it('separates a server that refuses to serve from one that cannot be reached', async () => {
    fetchMock.mockImplementation((url: string) => {
      const target = String(url);
      if (target.includes('/api/readiness')) {
        return Promise.resolve(
          jsonResponse({
            state: 'not-ready',
            reason: 'The service is not ready.',
            request_id: 'req-not-ready',
          }),
        );
      }
      return Promise.resolve(jsonResponse({}));
    });
    renderWithProviders(<HealthScreen />);

    expect(await screen.findByText('Not ready')).toBeTruthy();
    expect(screen.getByText(/running but reports it cannot serve/)).toBeTruthy();
    // The identifier is what correlates the screen with the server's own log.
    expect(screen.getByText(/req-not-ready/)).toBeTruthy();
    expect(screen.queryByText('Unreachable')).toBeNull();
  });

  it('reports an unreachable management API as unknown readiness, not as unready', async () => {
    fetchMock.mockImplementation((url: string) => {
      const target = String(url);
      if (target.includes('/api/readiness'))
        return Promise.reject(new TypeError('Failed to fetch'));
      return Promise.resolve(jsonResponse({}));
    });
    renderWithProviders(<HealthScreen />);

    expect(await screen.findByText('Unreachable')).toBeTruthy();
    expect(
      screen.getByText(/connectivity or process problem rather than a storage one/),
    ).toBeTruthy();
    expect(screen.queryByText('Not ready')).toBeNull();
  });
});
