import type * as React from 'react';

import { Card } from '@/components/ui/card';

/**
 * A single operational number.
 *
 * The value is always present as text. A chart, where one appears, is secondary
 * to the number it summarises.
 */
export function MetricCard({
  label,
  value,
  detail,
  footer,
}: {
  readonly label: string;
  readonly value: React.ReactNode;
  readonly detail?: React.ReactNode;
  readonly footer?: React.ReactNode;
}) {
  return (
    <Card className="p-4">
      <p className="text-xs font-medium text-ink-muted">{label}</p>
      <p className="mt-1 text-2xl font-semibold tabular-nums tracking-tight text-ink">{value}</p>
      {detail ? <p className="mt-0.5 text-xs text-ink-muted">{detail}</p> : null}
      {footer ? <div className="mt-3">{footer}</div> : null}
    </Card>
  );
}

/**
 * A capacity bar.
 *
 * The numeric value accompanies the bar, and the fill colour shifts only as a
 * secondary cue once utilisation becomes notable.
 */
export function UsageBar({
  used,
  total,
  label,
}: {
  readonly used: number;
  readonly total: number;
  readonly label: string;
}) {
  const percent = total > 0 ? Math.min(100, Math.round((used / total) * 100)) : 0;
  const tone = percent >= 95 ? 'bg-danger' : percent >= 80 ? 'bg-warn' : 'bg-accent';
  return (
    <div className="space-y-1">
      <div
        className="h-1.5 w-full overflow-hidden rounded-full bg-surface-muted"
        role="progressbar"
        aria-label={label}
        aria-valuenow={percent}
        aria-valuemin={0}
        aria-valuemax={100}
      >
        <div className={`h-full ${tone}`} style={{ width: `${percent}%` }} />
      </div>
      <p className="text-xs text-ink-subtle">{percent}% used</p>
    </div>
  );
}
