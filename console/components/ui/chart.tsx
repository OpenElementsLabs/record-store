'use client';

import * as React from 'react';
import * as Recharts from 'recharts';

import { cn } from '@/lib/utils';

/**
 * Chart configuration.
 *
 * Each series names itself and its colour, so a legend and a tooltip can be
 * generated from one declaration rather than repeated per component.
 */
export type ChartConfig = Record<
  string,
  {
    readonly label: string;
    /** A CSS colour, normally a design token. */
    readonly color: string;
  }
>;

const ChartContext = React.createContext<ChartConfig>({});

function useChartConfig(): ChartConfig {
  return React.useContext(ChartContext);
}

/**
 * The responsive frame every chart sits in.
 *
 * This stays deliberately thin: the caller provides the accessible name and a
 * minimum height, while the container supplies shadcn-style colour variables,
 * responsive sizing, and shared Recharts presentation rules.
 */
export function ChartContainer({
  config,
  className,
  children,
  ...props
}: {
  readonly config: ChartConfig;
  readonly className?: string;
  readonly children: React.ReactElement;
} & Omit<React.ComponentProps<'div'>, 'children'>) {
  const generatedId = React.useId();
  const chartId = `chart-${generatedId.replace(/:/g, '')}`;
  const colors = Object.fromEntries(
    Object.entries(config).map(([key, series]) => [`--color-${key}`, series.color]),
  ) as React.CSSProperties;

  return (
    <ChartContext.Provider value={config}>
      <div
        data-chart={chartId}
        className={cn(
          // This follows shadcn's composition model: the container owns the
          // responsive frame and series colour variables while Recharts owns
          // the visualization itself.
          'flex w-full justify-center text-xs [&_.recharts-cartesian-axis-tick_text]:fill-ink-muted [&_.recharts-cartesian-grid_line]:stroke-border [&_.recharts-curve.recharts-tooltip-cursor]:stroke-border-strong [&_.recharts-surface]:outline-none',
          className,
        )}
        {...props}
        style={{ ...colors, ...props.style }}
      >
        <Recharts.ResponsiveContainer width="100%" height="100%">
          {children}
        </Recharts.ResponsiveContainer>
      </div>
    </ChartContext.Provider>
  );
}

/** A tooltip that reads its labels and colours from the chart configuration. */
export function ChartTooltipContent({
  active,
  payload,
  label,
  formatter,
}: {
  readonly active?: boolean;
  readonly payload?: readonly { readonly dataKey?: string | number; readonly value?: unknown }[];
  readonly label?: unknown;
  /** Formats a value for display, so units stay the caller's decision. */
  readonly formatter?: (value: number) => string;
}) {
  const config = useChartConfig();
  if (!active || !payload || payload.length === 0) return null;

  return (
    <div className="rounded-control border border-border bg-surface-elevated px-2.5 py-2 shadow-md">
      {typeof label === 'string' ? <p className="type-meta-subtle">{label}</p> : null}
      <ul className="space-y-0.5">
        {payload.map((entry) => {
          const key = String(entry.dataKey ?? '');
          const series = config[key];
          const value = typeof entry.value === 'number' ? entry.value : Number(entry.value ?? 0);
          return (
            <li key={key} className="flex items-center gap-2">
              <span
                aria-hidden
                className="size-2 shrink-0 rounded-full"
                style={{ background: series?.color ?? 'var(--color-accent)' }}
              />
              <span className="type-meta">{series?.label ?? key}</span>
              <span className="ml-auto type-label tabular-nums">
                {formatter ? formatter(value) : value.toLocaleString('en-GB')}
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

export const ChartTooltip = Recharts.Tooltip;
export const Area = Recharts.Area;
export const AreaChart = Recharts.AreaChart;
export const CartesianGrid = Recharts.CartesianGrid;
export const XAxis = Recharts.XAxis;
export const YAxis = Recharts.YAxis;
