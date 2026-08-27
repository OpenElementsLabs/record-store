'use client';

import { useQuery } from '@tanstack/react-query';
import * as React from 'react';

import { queryKeys } from '@/hooks/use-system';
import { fetchSystemMetrics } from '@/lib/api/system';
import type { SystemMetrics } from '@/types/api';

/** How often the console reads the counters. */
export const SAMPLE_INTERVAL_MS = 15_000;

/** How many samples the rolling window keeps. */
const WINDOW = 40;

type Sample = { readonly at: number; readonly metrics: SystemMetrics };

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
  record(at: number, metrics: SystemMetrics): void {
    const latest = this.#samples[this.#samples.length - 1];
    if (latest?.at === at) return;
    this.#samples = [...this.#samples, { at, metrics }].slice(-WINDOW);
    for (const listener of this.#listeners) listener();
  }

  clear(): void {
    this.#samples = [];
    for (const listener of this.#listeners) listener();
  }
}

const store = new SampleStore();

/** Discards the observation window. Used by tests to isolate runs. */
export function resetMetricSamples(): void {
  store.clear();
}

/** A rate derived from two counter readings. */
export type Rate = {
  /** Units per second across the whole observed window. */
  readonly perSecond: number;
  /** Per-interval values, oldest first, for a trend line. */
  readonly series: readonly number[];
};

export type MetricsObservation = {
  readonly current: SystemMetrics | null;
  readonly isPending: boolean;
  readonly isFetching: boolean;
  readonly error: unknown;
  readonly refetch: () => void;
  /** When the newest sample was taken. */
  readonly observedAt: Date | null;
  /** How long the console has been watching, in seconds. */
  readonly windowSeconds: number;
  /** `null` until two samples exist: one counter reading is not a rate. */
  readonly requests: Rate | null;
  readonly errors: Rate | null;
  readonly uploadBytes: Rate | null;
  readonly downloadBytes: Rate | null;
};

function rateOf(samples: readonly Sample[], read: (metrics: SystemMetrics) => number): Rate | null {
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
  return { perSecond: total >= 0 ? total / elapsed : 0, series };
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
  const query = useQuery({
    queryKey: queryKeys.systemMetrics,
    queryFn: async ({ signal }) => {
      const metrics = await fetchSystemMetrics(signal);
      store.record(Date.now(), metrics);
      return metrics;
    },
    refetchInterval: SAMPLE_INTERVAL_MS,
  });

  const samples = React.useSyncExternalStore(store.subscribe, store.snapshot, store.snapshot);
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
