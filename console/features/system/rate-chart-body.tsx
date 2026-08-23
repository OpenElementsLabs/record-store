'use client';

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
  // Two points are the minimum that can describe a direction.
  if (series.length < 2) return <div className="h-24" aria-hidden />;

  const color = tone === 'danger' ? 'var(--color-danger)' : 'var(--color-accent)';
  const config: ChartConfig = { value: { label, color } };

  // Oldest first, labelled by how long ago the interval was measured.
  const seconds = Math.round(SAMPLE_INTERVAL_MS / 1000);
  const data = series.map((value, index) => ({
    at: `${(series.length - index) * seconds}s ago`,
    value,
  }));

  return (
    <ChartContainer config={config} className="h-24">
      <AreaChart data={data} margin={{ top: 4, right: 4, bottom: 0, left: 0 }}>
        <defs>
          <linearGradient id={`fill-${tone}`} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={color} stopOpacity={0.22} />
            <stop offset="100%" stopColor={color} stopOpacity={0.02} />
          </linearGradient>
        </defs>
        {/* Horizontal rules only: vertical lines would imply meaningful
            divisions between sampling intervals. */}
        <CartesianGrid vertical={false} stroke="var(--color-border)" strokeDasharray="2 4" />
        <XAxis dataKey="at" hide />
        <YAxis hide domain={[0, 'auto']} />
        <ChartTooltip
          cursor={{ stroke: 'var(--color-border-strong)', strokeWidth: 1 }}
          content={<ChartTooltipContent formatter={format} />}
        />
        <Area
          type="monotone"
          dataKey="value"
          stroke={color}
          strokeWidth={1.75}
          fill={`url(#fill-${tone})`}
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
