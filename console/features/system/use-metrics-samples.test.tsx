import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { fetchSystemMetrics, fetchSystemMetricsHistory } from '@/lib/api/system';
import type { SystemMetrics } from '@/types/api';
import { useMetricsSamples } from './use-metrics-samples';

vi.mock('@/lib/api/system', () => ({
  fetchSystemMetrics: vi.fn(),
  fetchSystemMetricsHistory: vi.fn(),
}));

/** A server history of `count` readings, `interval` seconds apart, ending now. */
const history = (count: number, interval = 15, step = 100) => ({
  interval_seconds: interval,
  capacity: 240,
  started_at: new Date(Date.now() - count * interval * 1000).toISOString(),
  samples: Array.from({ length: count }, (_, index) => ({
    at: new Date(Date.now() - (count - 1 - index) * interval * 1000).toISOString(),
    requests: index * step,
    errors: 0,
    upload_bytes: 0,
    download_bytes: 0,
  })),
});
const reading = (requests: number) =>
  ({
    requests,
    errors: 0,
    upload_bytes: 0,
    download_bytes: 0,
    storage: {
      object_count: 0,
      bucket_count: 0,
      version_count: 0,
      logical_bytes: 0,
      physical_bytes: 0,
      multipart_bytes: 0,
    },
  }) satisfies SystemMetrics;

function mount(
  client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } }),
) {
  return renderHook(() => useMetricsSamples(), {
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    ),
  });
}

afterEach(() => {
  vi.useRealTimers();
  vi.resetAllMocks();
});

describe('metrics sampling flow', () => {
  it('measures a rate after two seconds, then slows down after startup', async () => {
    vi.useFakeTimers();
    vi.mocked(fetchSystemMetrics)
      .mockResolvedValueOnce(reading(100))
      .mockResolvedValue(reading(120));
    const { result } = mount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(result.current.requests).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_010);
    });
    expect(result.current.requests?.perSecond).toBeCloseTo(10, 0);
    expect(fetchSystemMetrics).toHaveBeenCalledTimes(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_010);
    });
    expect(fetchSystemMetrics).toHaveBeenCalledTimes(3);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(fetchSystemMetrics).toHaveBeenCalledTimes(3);
  });

  it('uses a cached overview reading with its actual timestamp', async () => {
    vi.useFakeTimers();
    const client = new QueryClient();
    client.setQueryData(['system', 'metrics'], reading(100), { updatedAt: Date.now() - 5_000 });
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(150));
    const { result } = mount(client);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(result.current.requests?.perSecond).toBe(10);
    expect(result.current.requests?.secondsAgo).toEqual([0]);
    expect(fetchSystemMetrics).toHaveBeenCalledTimes(1);
    client.clear();
  });

  it('does not reuse history from a different query client', async () => {
    vi.useFakeTimers();
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(10));
    const first = mount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2_010);
    });
    expect(first.result.current.requests).not.toBeNull();
    first.unmount();
    const second = mount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(second.result.current.requests).toBeNull();
  });
});

describe('seeding from the server history', () => {
  /**
   * The reason this exists. Before seeding, opening Metrics showed nothing until
   * the console had personally watched two readings go by, and a full trend took
   * minutes. The server has been sampling since it started, so the first paint
   * can show a window somebody already waited for.
   */
  it('reports a rate on the first reading instead of waiting to observe two', async () => {
    vi.mocked(fetchSystemMetricsHistory).mockResolvedValue(history(5) as never);
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(400));

    const { result } = mount();
    await vi.waitFor(() => expect(result.current.requests).not.toBeNull());

    // Five readings fifteen seconds apart at 100 requests each: 100/15 per
    // second. Checked loosely because the live reading lands at real
    // wall-clock time rather than exactly on the seeded grid.
    expect(result.current.requests?.perSecond).toBeCloseTo(100 / 15, 1);
    expect(result.current.windowSeconds).toBeGreaterThanOrEqual(60);
    expect(result.current.requests?.series.length).toBeGreaterThanOrEqual(4);
  });

  /**
   * Once this client has started watching, its own readings are the record.
   * Splicing server samples in underneath them would interleave two clocks and
   * could produce a rate that never happened.
   */
  it('does not seed a window that already holds observations', async () => {
    let resolveHistory: (value: unknown) => void = () => {};
    vi.mocked(fetchSystemMetricsHistory).mockReturnValue(
      new Promise((resolve) => {
        resolveHistory = resolve;
      }) as never,
    );
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(10));

    const { result } = mount();
    await vi.waitFor(() => expect(result.current.current).not.toBeNull());

    // The history arrives late, after the console has already read a counter.
    await act(async () => {
      resolveHistory(history(5));
      await Promise.resolve();
    });

    expect(result.current.windowSeconds).toBe(0);
    expect(result.current.requests).toBeNull();
  });

  /**
   * Coming from Overview, its cached reading reaches the window before the
   * history does. A server that names its clock lets the history go in
   * underneath that reading anyway, moved onto the browser's clock, so a skew
   * between the two machines changes nothing about the rate.
   */
  it('seeds beneath an existing reading when the server names its clock, whatever the skew', async () => {
    const skew = 90_000; // the server's clock runs a minute and a half ahead
    const seeded = history(5);
    const shifted = {
      ...seeded,
      now: new Date(Date.now() + skew).toISOString(),
      samples: seeded.samples.map((sample) => ({
        ...sample,
        at: new Date(Date.parse(sample.at) + skew - 1_500).toISOString(),
      })),
    };
    let resolveHistory: (value: unknown) => void = () => {};
    vi.mocked(fetchSystemMetricsHistory).mockReturnValue(
      new Promise((resolve) => {
        resolveHistory = resolve;
      }) as never,
    );
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(415));

    const { result } = mount();
    await vi.waitFor(() => expect(result.current.current).not.toBeNull());
    await act(async () => {
      resolveHistory(shifted);
      await Promise.resolve();
    });

    await vi.waitFor(() => expect(result.current.requests).not.toBeNull());
    // Five readings 15 s apart at 100 requests each, then the live one 1.5 s
    // later at +15: every interval is 100/15 per second. Had the skew leaked
    // in, the last interval would span -88.5 s and the window would be wrong.
    expect(result.current.requests?.perSecond).toBeCloseTo(415 / 61.5, 1);
    expect(result.current.windowSeconds).toBeGreaterThanOrEqual(60);
    expect(result.current.windowSeconds).toBeLessThanOrEqual(63);
  });

  /**
   * Seeding is a convenience. A server too old to know the path, or a proxy
   * returning something else entirely, must leave the screen working exactly as
   * it did before — not blank it.
   */
  it('keeps working when the history is missing or malformed', async () => {
    for (const response of [
      undefined,
      null,
      {},
      { samples: null },
      { samples: 'nope' },
      { samples: [{ at: 'not-a-date', requests: 1 }] },
      { samples: [{ at: new Date().toISOString(), requests: 'lots' }] },
    ]) {
      vi.resetAllMocks();
      vi.mocked(fetchSystemMetricsHistory).mockResolvedValue(response as never);
      vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(7));

      const { result, unmount } = mount();
      await vi.waitFor(() => expect(result.current.current).not.toBeNull());

      expect(result.current.current?.requests).toBe(7);
      expect(result.current.requests).toBeNull();
      unmount();
    }
  });

  /** A rejected history is not an error the screen should show. */
  it('reports no error when the history cannot be read', async () => {
    vi.mocked(fetchSystemMetricsHistory).mockRejectedValue(new Error('no such route'));
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(3));

    const { result } = mount();
    await vi.waitFor(() => expect(result.current.current).not.toBeNull());

    expect(result.current.error).toBeFalsy();
    expect(result.current.current?.requests).toBe(3);
  });

  /**
   * Counters only move forward in time. A repeated or reordered timestamp would
   * otherwise divide a real delta by zero or negative elapsed seconds.
   */
  it('normalises reordered and duplicated readings', async () => {
    const now = Date.now();
    const at = (offset: number) => new Date(now + offset).toISOString();
    vi.mocked(fetchSystemMetricsHistory).mockResolvedValue({
      interval_seconds: 15,
      capacity: 240,
      started_at: at(-30_000),
      samples: [
        { at: at(-15_000), requests: 150, errors: 0, upload_bytes: 0, download_bytes: 0 },
        { at: at(-30_000), requests: 0, errors: 0, upload_bytes: 0, download_bytes: 0 },
        { at: at(-15_000), requests: 999, errors: 0, upload_bytes: 0, download_bytes: 0 },
      ],
    } as never);
    vi.mocked(fetchSystemMetrics).mockResolvedValue(reading(300));

    const { result } = mount();
    await vi.waitFor(() => expect(result.current.requests).not.toBeNull());

    // Three samples survive — two seeded plus the live reading — so the extra
    // intervals the duplicate would have added are not there. Asserting the
    // count proves the dedup; the rate is checked loosely because the live
    // reading lands at real wall-clock time, not exactly on the grid.
    expect(result.current.requests?.series).toHaveLength(2);
    expect(result.current.requests?.perSecond).toBeCloseTo(10, 1);
    // Every interval is a real forward-moving delta, never a divide by zero.
    for (const value of result.current.requests?.series ?? []) {
      expect(Number.isFinite(value)).toBe(true);
      expect(value).toBeGreaterThan(0);
    }
  });
});
