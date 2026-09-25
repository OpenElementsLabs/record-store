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
/** Counter bodies to answer with, in order, before falling back to the default. */
let metricsQueue: unknown[];
/** Answer for every counter read once the queue is empty; null makes it fail. */
let metricsDefault: unknown | null;

/** Queues one counter reading. */
function queueMetrics(body: unknown): void {
  metricsQueue.push(body);
}

/** Sets the counter reading every later read returns. */
function defaultMetrics(body: unknown): void {
  metricsDefault = body;
}

/** Makes every later counter read fail. */
function failMetrics(): void {
  metricsDefault = null;
  metricsQueue = [];
}

beforeEach(() => {
  resetMetricSamples();
  metricsQueue = [];
  metricsDefault = null;
  // Routed by URL rather than by call order. The screen reads its counters and,
  // once, the server's sample history; an order-dependent mock would hand one
  // endpoint the other's answer depending on which resolved first.
  fetchMock = vi.fn(async (input: unknown) => {
    const url = String(input);
    if (url.includes('/system/metrics/history')) {
      // Empty by default, so these tests measure what the console observes for
      // itself. Seeding has its own tests.
      return jsonResponse({
        interval_seconds: 15,
        capacity: 240,
        started_at: new Date(0).toISOString(),
        samples: [],
      });
    }
    const body = metricsQueue.shift() ?? metricsDefault;
    if (body === null || body === undefined) throw new TypeError('Failed to fetch');
    return jsonResponse(body);
  });
  vi.stubGlobal('fetch', fetchMock);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('MetricsScreen', () => {
  it('will not report a rate from a single reading', async () => {
    defaultMetrics(metrics());
    renderWithProviders(<MetricsScreen />);

    // One counter value is not a rate. Showing 0 req/s here would read as an
    // idle server rather than as "not measured yet".
    expect(await screen.findAllByText('Collecting…')).toHaveLength(4);
    expect(document.body.textContent ?? '').not.toMatch(/0 req\/s/);
  });

  it('derives a rate from the difference between two readings', async () => {
    let now = 1_000_000;
    vi.spyOn(Date, 'now').mockImplementation(() => now);
    queueMetrics(metrics({ requests: 1_000 }));

    const { client } = renderWithProviders(<MetricsScreen />);
    await screen.findByText('1,000 total since start');

    // Ten seconds later, 200 more requests: 20 req/s.
    now += 10_000;
    defaultMetrics(metrics({ requests: 1_200 }));
    await client.refetchQueries({ queryKey: ['system', 'metrics'] });

    expect(await screen.findByText('20 req/s')).toBeTruthy();
    expect(screen.getByText(/1,200 total since start/)).toBeTruthy();
  });

  it('treats a counter reset as zero rather than a negative rate', async () => {
    let now = 2_000_000;
    vi.spyOn(Date, 'now').mockImplementation(() => now);
    queueMetrics(metrics({ requests: 5_000 }));

    const { client } = renderWithProviders(<MetricsScreen />);
    await screen.findByText('5,000 total since start');

    // The server restarted, so its counters went backwards.
    now += 10_000;
    defaultMetrics(metrics({ requests: 3 }));
    await client.refetchQueries({ queryKey: ['system', 'metrics'] });

    await waitFor(() => expect(screen.getByText('0.00 req/s')).toBeTruthy());
    expect(document.body.textContent ?? '').not.toMatch(/-\d/);
  });

  it('reports physical storage against logical rather than as a bare number', async () => {
    defaultMetrics(metrics());
    renderWithProviders(<MetricsScreen />);

    expect(await screen.findByText('3.00 GB')).toBeTruthy();
    // 3 GB physical for 1 GB logical is 300% — the overhead is the useful part.
    expect(screen.getByText('300% of logical')).toBeTruthy();
  });

  it('shows no cluster section in a standalone deployment', async () => {
    defaultMetrics(metrics());
    renderWithProviders(<MetricsScreen />, { info: systemInfo({ mode: 'standalone' }) });

    await screen.findByText('Traffic');
    expect(screen.queryByText('Metadata quorum')).toBeNull();
    expect(screen.queryByText('Under-replicated')).toBeNull();
  });

  it('shows durability figures when the backend reports a cluster', async () => {
    defaultMetrics(
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
    );
    renderWithProviders(<MetricsScreen />);

    expect(await screen.findByText('Writable')).toBeTruthy();
    expect(screen.getByText('Under-replicated')).toBeTruthy();
    expect(screen.getByLabelText('Node disk utilisation').getAttribute('aria-valuenow')).toBe('25');
  });

  it('keeps the last successful values visible when refresh fails', async () => {
    queueMetrics(metrics());
    const { client } = renderWithProviders(<MetricsScreen />);
    await screen.findByText('3.00 GB');
    failMetrics();
    await client.refetchQueries({ queryKey: ['system', 'metrics'] });
    expect((await screen.findByRole('alert')).textContent).toContain(
      'Showing the last successful readings',
    );
    expect(screen.getByText('3.00 GB')).toBeTruthy();
    expect(screen.getByText('Traffic')).toBeTruthy();
    defaultMetrics(metrics());
    await client.refetchQueries({ queryKey: ['system', 'metrics'] });
    await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
  });

  it('re-reads the counters on request', async () => {
    defaultMetrics(metrics());
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
