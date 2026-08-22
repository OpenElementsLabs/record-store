import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { MetricsScreen } from './metrics-screen';
import { resetMetricSamples } from './use-metrics-samples';
import { jsonResponse, renderWithProviders, systemInfo } from '@/test/render';
import type { SystemMetrics } from '@/types/api';

function metrics(overrides: Partial<SystemMetrics> = {}): SystemMetrics {
  return {
    requests: 1_000,
    errors: 10,
    upload_bytes: 2_000_000,
    download_bytes: 4_000_000,
    storage: {
      object_count: 42,
      bucket_count: 3,
      version_count: 50,
      logical_bytes: 1_000_000_000,
      physical_bytes: 3_000_000_000,
      multipart_bytes: 0,
    },
    ...overrides,
  };
}

let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  resetMetricSamples();
  fetchMock = vi.fn();
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('MetricsScreen', () => {
  it('will not report a rate from a single reading', async () => {
    fetchMock.mockResolvedValue(jsonResponse(metrics()));
    renderWithProviders(<MetricsScreen />);

    // One counter value is not a rate. Showing 0 req/s here would read as an
    // idle server rather than as "not measured yet".
    expect(await screen.findAllByText('Collecting…')).toHaveLength(4);
    expect(document.body.textContent ?? '').not.toMatch(/0 req\/s/);
  });

  it('derives a rate from the difference between two readings', async () => {
    let now = 1_000_000;
    vi.spyOn(Date, 'now').mockImplementation(() => now);
    fetchMock.mockResolvedValueOnce(jsonResponse(metrics({ requests: 1_000 })));

    const { client } = renderWithProviders(<MetricsScreen />);
    await screen.findAllByText('Collecting…');

    // Ten seconds later, 200 more requests: 20 req/s.
    now += 10_000;
    fetchMock.mockResolvedValue(jsonResponse(metrics({ requests: 1_200 })));
    await client.refetchQueries({ queryKey: ['system', 'metrics'] });

    expect(await screen.findByText('20 req/s')).toBeTruthy();
    expect(screen.getByText(/1,200 total since start/)).toBeTruthy();
  });

  it('treats a counter reset as zero rather than a negative rate', async () => {
    let now = 2_000_000;
    vi.spyOn(Date, 'now').mockImplementation(() => now);
    fetchMock.mockResolvedValueOnce(jsonResponse(metrics({ requests: 5_000 })));

    const { client } = renderWithProviders(<MetricsScreen />);
    await screen.findAllByText('Collecting…');

    // The server restarted, so its counters went backwards.
    now += 10_000;
    fetchMock.mockResolvedValue(jsonResponse(metrics({ requests: 3 })));
    await client.refetchQueries({ queryKey: ['system', 'metrics'] });

    await waitFor(() => expect(screen.getByText('0.00 req/s')).toBeTruthy());
    expect(document.body.textContent ?? '').not.toMatch(/-\d/);
  });

  it('reports physical storage against logical rather than as a bare number', async () => {
    fetchMock.mockResolvedValue(jsonResponse(metrics()));
    renderWithProviders(<MetricsScreen />);

    expect(await screen.findByText('3.00 GB')).toBeTruthy();
    // 3 GB physical for 1 GB logical is 300% — the overhead is the useful part.
    expect(screen.getByText('300% of logical')).toBeTruthy();
  });

  it('shows no cluster section in a standalone deployment', async () => {
    fetchMock.mockResolvedValue(jsonResponse(metrics()));
    renderWithProviders(<MetricsScreen />, { info: systemInfo({ mode: 'standalone' }) });

    await screen.findByText('Traffic');
    expect(screen.queryByText('Metadata quorum')).toBeNull();
    expect(screen.queryByText('Under-replicated')).toBeNull();
  });

  it('shows durability figures when the backend reports a cluster', async () => {
    fetchMock.mockResolvedValue(
      jsonResponse(
        metrics({
          cluster: {
            nodes: 3,
            healthy: true,
            quorum_writable: true,
            under_replicated_objects: 4,
            repair_active_tasks: 1,
            node_capacity_bytes: 1_000_000_000,
            node_used_bytes: 250_000_000,
            node_available_bytes: 750_000_000,
            logical_bytes: 1_000_000_000,
            physical_bytes: 3_000_000_000,
          },
        }),
      ),
    );
    renderWithProviders(<MetricsScreen />);

    expect(await screen.findByText('Writable')).toBeTruthy();
    expect(screen.getByText('Under-replicated')).toBeTruthy();
    expect(screen.getByLabelText('Node disk utilisation').getAttribute('aria-valuenow')).toBe('25');
  });

  it('re-reads the counters on request', async () => {
    fetchMock.mockResolvedValue(jsonResponse(metrics()));
    renderWithProviders(<MetricsScreen />);
    await screen.findByText('Traffic');
    await waitFor(() =>
      expect(screen.getByRole('button', { name: 'Refresh metrics' }).hasAttribute('disabled')).toBe(
        false,
      ),
    );
    const before = fetchMock.mock.calls.length;

    await userEvent.click(screen.getByRole('button', { name: 'Refresh metrics' }));

    await waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThan(before));
  });
});
