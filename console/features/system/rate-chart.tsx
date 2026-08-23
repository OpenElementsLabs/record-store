'use client';

import dynamic from 'next/dynamic';

/**
 * The chart, loaded only when a screen actually renders one.
 *
 * Recharts is the largest dependency in the console and exactly one screen
 * needs it, so it is fetched on demand rather than shipped to everyone who
 * signs in. The placeholder reserves the chart's height so nothing shifts when
 * it arrives.
 */
const RateChartBody = dynamic(
  () => import('@/features/system/rate-chart-body').then((module) => module.RateChartBody),
  { ssr: false, loading: () => <div className="h-24" aria-hidden /> },
);

export function RateChart(props: React.ComponentProps<typeof RateChartBody>) {
  return <RateChartBody {...props} />;
}
