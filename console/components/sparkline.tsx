/**
 * A small inline trend line.
 *
 * Deliberately dependency-free: a handful of sparklines does not justify a
 * charting library, and the number they annotate is always shown as text
 * beside them. The line is decoration for a value, never the value itself.
 */
export function Sparkline({
  values,
  label,
  tone = 'accent',
}: {
  readonly values: readonly number[];
  readonly label: string;
  readonly tone?: 'accent' | 'danger';
}) {
  // Two points are the minimum that can describe a direction.
  if (values.length < 2) {
    return <div className="h-8" aria-hidden />;
  }
  const width = 100;
  const height = 28;
  const highest = Math.max(...values);
  // A flat series sits on the baseline rather than dividing by zero.
  const scale = highest > 0 ? highest : 1;
  const step = width / (values.length - 1);
  const points = values.map((value, index) => {
    const x = index * step;
    const y = height - (value / scale) * (height - 2) - 1;
    return `${x.toFixed(2)},${y.toFixed(2)}`;
  });
  const stroke = tone === 'danger' ? 'var(--color-danger)' : 'var(--color-accent)';

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      className="h-8 w-full"
      role="img"
      aria-label={label}
    >
      <polyline
        points={`0,${height} ${points.join(' ')} ${width},${height}`}
        fill={stroke}
        fillOpacity="0.08"
        stroke="none"
      />
      <polyline
        points={points.join(' ')}
        fill="none"
        stroke={stroke}
        strokeWidth="1.5"
        strokeLinejoin="round"
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
