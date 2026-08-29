'use client';

import * as React from 'react';

import {
  Area,
  AreaChart,
  CartesianGrid,
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  XAxis,
  YAxis,
  type ChartConfig,
} from '@/components/ui/chart';
import { SAMPLE_INTERVAL_MS } from '@/features/system/use-metrics-samples';

/**
 * The observed history of one derived rate.
 *
 * The number this annotates is always shown as text beside it. The chart adds
 * the shape of the last few minutes, which a single figure cannot carry, and it
 * plots only intervals actually measured — there is no interpolation across a
 * gap and no smoothing that would invent a value.
 */
export function RateChartBody({
  series,
  label,
  tone = 'accent',
  format,
}: {
  readonly series: readonly number[];
  readonly label: string;
  readonly tone?: 'accent' | 'danger';
  readonly format: (value: number) => string;
}) {
  const gradientId = React.useId().replace(/:/g, '');

  // Two points are the minimum that can describe a direction.
  if (series.length < 2) {
    return (
      <div className="flex min-h-60 items-center justify-center rounded-control border border-dashed border-border bg-surface-subtle/40 px-4 text-center type-meta-subtle">
        Waiting for another sample to draw the trend.
      </div>
    );
  }

  const color = tone === 'danger' ? 'var(--color-danger)' : 'var(--color-accent)';
  const config: ChartConfig = { value: { label, color } };

  // Oldest first, labelled by how long ago the interval was measured.
  const seconds = Math.round(SAMPLE_INTERVAL_MS / 1000);
  const data = series.map((value, index) => ({
    secondsAgo: (series.length - 1 - index) * seconds,
    value,
  }));

  return (
    <ChartContainer
      config={config}
      className="min-h-60"
      aria-label={`${label} rate over the observed window`}
    >
      <AreaChart accessibilityLayer data={data} margin={{ top: 10, right: 8, bottom: 0, left: 0 }}>
        <defs>
          <linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="var(--color-value)" stopOpacity={0.28} />
            <stop offset="100%" stopColor="var(--color-value)" stopOpacity={0.025} />
          </linearGradient>
        </defs>
        {/* Horizontal rules only: vertical lines would imply meaningful
            divisions between sampling intervals. */}
        <CartesianGrid vertical={false} strokeDasharray="2 4" />
        <XAxis
          dataKey="secondsAgo"
          axisLine={false}
          tickLine={false}
          tickMargin={10}
          minTickGap={32}
          tickFormatter={formatElapsed}
        />
        <YAxis
          axisLine={false}
          tickLine={false}
          tickMargin={8}
          width={46}
          domain={[0, 'auto']}
          tickFormatter={formatAxisValue}
        />
        <ChartTooltip
          cursor={{ stroke: 'var(--color-border-strong)', strokeWidth: 1 }}
          labelFormatter={(value) => formatElapsed(Number(value))}
          content={<ChartTooltipContent formatter={format} />}
        />
        <Area
          type="monotone"
          dataKey="value"
          stroke="var(--color-value)"
          strokeWidth={2}
          fill={`url(#${gradientId})`}
          // A dot per sample would clutter a 40-point series; the hover cursor
          // is how a single interval is inspected.
          dot={false}
          activeDot={{ r: 3, strokeWidth: 0 }}
          isAnimationActive={false}
        />
      </AreaChart>
    </ChartContainer>
  );
}

function formatElapsed(seconds: number): string {
  if (seconds <= 0) return 'Now';
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  return `${minutes}m ago`;
}

const compactNumber = new Intl.NumberFormat('en-GB', {
  notation: 'compact',
  maximumFractionDigits: 1,
});

function formatAxisValue(value: number): string {
  return compactNumber.format(value);
}
