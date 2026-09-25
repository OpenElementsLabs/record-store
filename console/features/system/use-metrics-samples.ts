'use client';

import { QueryClient, useQuery, useQueryClient } from '@tanstack/react-query';
import * as React from 'react';

import { queryKeys } from '@/hooks/use-system';
import { fetchSystemMetrics, fetchSystemMetricsHistory } from '@/lib/api/system';
import type { MetricsHistory, SystemMetrics } from '@/types/api';

/** How often the console reads the counters. */
export const SAMPLE_INTERVAL_MS = 15_000;
/** Take the first few readings promptly, then return to normal polling. */
export const STARTUP_INTERVAL_MS = 2_000;

/**
 * How many samples the rolling window keeps.
 *
 * Matches the server's retention, so seeding cannot hand the window more than
 * it is willing to hold.
 */
const WINDOW = 240;

/**
 * A seeded reading must precede this client's first by at least this much, so
 * the interval between them is a measurement rather than rounding and latency.
 */
const MINIMUM_SEED_GAP_MS = 1_000;

/**
 * The four counters the charts differentiate.
 *
 * Narrower than `SystemMetrics` on purpose. A seeded reading comes from the
 * server's history, which carries counters and nothing else, and widening this
 * to the full shape would mean inventing storage figures that were never read.
 */
type CounterReading = Pick<
  SystemMetrics,
  'requests' | 'errors' | 'upload_bytes' | 'download_bytes'
>;

type Sample = { readonly at: number; readonly metrics: CounterReading };

/**
 * The observation window, held outside React.
 *
 * Counter readings arrive from the network, which makes them an external
 * event rather than state derived from a render. Keeping them in a store and
 * subscribing is what lets the window survive navigation between screens, so
 * returning to Metrics does not restart the measurement from nothing.
 */
class SampleStore {
  #samples: readonly Sample[] = [];
  #listeners = new Set<() => void>();

  subscribe = (listener: () => void): (() => void) => {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  };

  snapshot = (): readonly Sample[] => this.#samples;

  /** Records one reading, ignoring a repeat of the newest one. */
  record(at: number, metrics: CounterReading): void {
    const latest = this.#samples[this.#samples.length - 1];
    if (latest && latest.at >= at) return;
    this.#samples = [...this.#samples, { at, metrics }].slice(-WINDOW);
    for (const listener of this.#listeners) listener();
  }

  /**
   * Fills the window with readings the server already took.
   *
   * An empty window takes them as they are. A window this client has already
   * started filling takes only readings older than its own, and only when they
   * were `aligned` onto this client's clock: splicing server-clock samples in
   * underneath browser-clock ones would make a rate out of the clock skew.
   */
  seed(samples: readonly Sample[], aligned: boolean): void {
    if (samples.length === 0) return;
    const first = this.#samples[0];
    if (!first) {
      this.#samples = samples.slice(-WINDOW);
    } else if (aligned) {
      const older = samples.filter((sample) => sample.at < first.at - MINIMUM_SEED_GAP_MS);
      if (older.length === 0) return;
      this.#samples = [...older, ...this.#samples].slice(-WINDOW);
    } else {
      return;
    }
    for (const listener of this.#listeners) listener();
  }

  clear(): void {
    this.#samples = [];
    for (const listener of this.#listeners) listener();
  }
}

let stores = new WeakMap<QueryClient, SampleStore>();

function sampleStore(client: QueryClient): SampleStore {
  let store = stores.get(client);
  if (!store) {
    store = new SampleStore();
    stores.set(client, store);
  }
  return store;
}

/** Discards the observation window. Used by tests to isolate runs. */
export function resetMetricSamples(): void {
  stores = new WeakMap();
}

/** A rate derived from two counter readings. */
export type Rate = {
  /** Units per second across the whole observed window. */
  readonly perSecond: number;
  /** Per-interval values, oldest first, for a trend line. */
  readonly series: readonly number[];
  /** Actual reading times; refreshes and slow responses need not be evenly spaced. */
  readonly secondsAgo: readonly number[];
};

export type MetricsObservation = {
  readonly current: SystemMetrics | null;
  readonly isPending: boolean;
  readonly isFetching: boolean;
  readonly error: unknown;
  readonly refetch: () => void;
  /** When the newest sample was taken. */
  readonly observedAt: Date | null;
  /**
   * Span the rates cover, in seconds.
   *
   * Includes readings the server took before this screen was opened, so it is
   * the observed window rather than this client's uptime.
   */
  readonly windowSeconds: number;
  /** `null` until two samples exist: one counter reading is not a rate. */
  readonly requests: Rate | null;
  readonly errors: Rate | null;
  readonly uploadBytes: Rate | null;
  readonly downloadBytes: Rate | null;
};

/** Reads a counter, rejecting anything that is not a usable number. */
function counter(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
}

/**
 * Converts the server's readings into window samples.
 *
 * Every field is checked rather than trusted. This body crosses a network and a
 * proxy, and it may come from a server older than this console that answers the
 * path with something else entirely. Seeding is a convenience — it must never be
 * able to take down the screen it was only meant to fill in, so anything
 * unrecognisable is dropped and the screen falls back to watching for itself.
 *
 * Timestamps are the server's, parsed to epoch milliseconds, so a seeded sample
 * keeps the instant it was actually taken. When the server says what its clock
 * read as it answered, every timestamp is moved onto this client's clock by
 * the difference, measured against `receivedAt`, and the result is `aligned`.
 */
function seedSamples(
  history: MetricsHistory | undefined,
  receivedAt: number,
): { samples: readonly Sample[]; aligned: boolean } {
  const source: unknown = history?.samples;
  if (!Array.isArray(source)) return { samples: [], aligned: false };
  const serverNow = typeof history?.now === 'string' ? Date.parse(history.now) : Number.NaN;
  const aligned = !Number.isNaN(serverNow) && receivedAt > 0;
  const offset = aligned ? receivedAt - serverNow : 0;
  const samples: Sample[] = [];
  for (const entry of source) {
    if (typeof entry !== 'object' || entry === null) continue;
    const reading = entry as Record<string, unknown>;
    const at = typeof reading.at === 'string' ? Date.parse(reading.at) : Number.NaN;
    const requests = counter(reading.requests);
    const errors = counter(reading.errors);
    const uploadBytes = counter(reading.upload_bytes);
    const downloadBytes = counter(reading.download_bytes);
    if (
      Number.isNaN(at) ||
      requests === null ||
      errors === null ||
      uploadBytes === null ||
      downloadBytes === null
    ) {
      continue;
    }
    samples.push({
      at: at + offset,
      metrics: {
        requests,
        errors,
        upload_bytes: uploadBytes,
        download_bytes: downloadBytes,
      },
    });
  }
  // Oldest first, and strictly increasing, whatever order they arrived in: a
  // repeated or reordered timestamp would make a rate out of a delta that never
  // elapsed.
  samples.sort((left, right) => left.at - right.at);
  return {
    samples: samples.filter((sample, index) => index === 0 || sample.at > samples[index - 1]!.at),
    aligned,
  };
}

function rateOf(
  samples: readonly Sample[],
  read: (metrics: CounterReading) => number,
): Rate | null {
  if (samples.length < 2) return null;
  const first = samples[0] as Sample;
  const last = samples[samples.length - 1] as Sample;
  const elapsed = (last.at - first.at) / 1000;
  if (elapsed <= 0) return null;

  const series: number[] = [];
  for (let index = 1; index < samples.length; index += 1) {
    const previous = samples[index - 1] as Sample;
    const current = samples[index] as Sample;
    const seconds = (current.at - previous.at) / 1000;
    const delta = read(current.metrics) - read(previous.metrics);
    // A negative delta means the server restarted and its counters reset.
    // Reporting a negative rate would be nonsense, so the interval reads zero.
    series.push(seconds > 0 && delta >= 0 ? delta / seconds : 0);
  }
  const total = read(last.metrics) - read(first.metrics);
  return {
    perSecond: total >= 0 ? total / elapsed : 0,
    series,
    secondsAgo: samples.slice(1).map((sample) => (last.at - sample.at) / 1000),
  };
}

/**
 * Polls the counters and differentiates them into rates.
 *
 * Record Store exposes counters, not rates, so a rate can only come from comparing two
 * readings — which is what a scraper does too. The window is however long the
 * console has been observing, and the screen says so rather than implying a
 * server-side average.
 */
export function useMetricsSamples(): MetricsObservation {
  const client = useQueryClient();
  const store = sampleStore(client);
  const samples = React.useSyncExternalStore(store.subscribe, store.snapshot, store.snapshot);

  // The server has been reading its own counters since it started, so the first
  // paint can show a window somebody already waited for instead of an empty
  // chart -- also when Overview's cached reading got there first. Fetched once:
  // after that this client's own readings are the record.
  const history = useQuery({
    queryKey: queryKeys.systemMetricsHistory,
    queryFn: ({ signal }) => fetchSystemMetricsHistory(signal),
    enabled: samples.length < 3,
    staleTime: Infinity,
    // A missing history is not an error worth showing. Without it the screen
    // behaves exactly as it did before: it starts empty and fills as it watches.
    retry: false,
  });

  React.useEffect(() => {
    if (!history.data) return;
    const seeded = seedSamples(history.data, history.dataUpdatedAt);
    store.seed(seeded.samples, seeded.aligned);
  }, [history.data, history.dataUpdatedAt, store]);
  const query = useQuery({
    queryKey: queryKeys.systemMetrics,
    queryFn: ({ signal }) => fetchSystemMetrics(signal),
    staleTime: 0,
    refetchOnMount: 'always',
    refetchInterval: (query) =>
      query.state.status !== 'error' && samples.length < 3
        ? STARTUP_INTERVAL_MS
        : SAMPLE_INTERVAL_MS,
    // Report failed reads promptly. Polling and manual refresh can recover them.
    retry: false,
  });

  // Include readings already fetched by Overview, retaining their real timestamp.
  // Recording on query updates also avoids missing a shared, in-flight request.
  React.useEffect(() => {
    if (query.data && query.dataUpdatedAt) store.record(query.dataUpdatedAt, query.data);
  }, [query.data, query.dataUpdatedAt, store]);

  const first = samples[0];
  const last = samples[samples.length - 1];

  return {
    current: query.data ?? null,
    isPending: query.isPending,
    isFetching: query.isFetching,
    error: query.error,
    refetch: () => void query.refetch(),
    observedAt: last ? new Date(last.at) : null,
    windowSeconds: first && last ? Math.max(0, Math.round((last.at - first.at) / 1000)) : 0,
    requests: rateOf(samples, (metrics) => metrics.requests),
    errors: rateOf(samples, (metrics) => metrics.errors),
    uploadBytes: rateOf(samples, (metrics) => metrics.upload_bytes),
    downloadBytes: rateOf(samples, (metrics) => metrics.download_bytes),
  };
}
