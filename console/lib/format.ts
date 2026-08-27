/**
 * Presentation helpers shared by every screen.
 *
 * Formatting lives in one place so the same number never appears with two
 * different unit conventions in two different components.
 */

/**
 * Decimal byte units.
 *
 * Record Store reports raw byte counts. The console renders them in decimal units
 * throughout, matching how storage capacity is normally quoted, and never mixes
 * decimal and binary prefixes.
 */
const BYTE_UNITS = ['B', 'kB', 'MB', 'GB', 'TB', 'PB', 'EB'] as const;

export type FormatBytesOptions = {
  /** Significant fractional digits for values above one kilobyte. */
  readonly precision?: number;
};

/** Renders a byte count using decimal units, for example `1.82 TB`. */
export function formatBytes(bytes: number, options: FormatBytesOptions = {}): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—';
  if (bytes < 1000) return `${Math.round(bytes)} B`;

  let value = bytes;
  let unit = 0;
  while (value >= 1000 && unit < BYTE_UNITS.length - 1) {
    value /= 1000;
    unit += 1;
  }
  const precision = options.precision ?? (value < 10 ? 2 : value < 100 ? 1 : 0);
  return `${value.toFixed(precision)} ${BYTE_UNITS[unit]}`;
}

/** Renders a used-of-total pair, for example `1.82 TB of 4.00 TB`. */
export function formatBytesOf(used: number, total: number): string {
  if (total <= 0) return formatBytes(used);
  return `${formatBytes(used)} of ${formatBytes(total)}`;
}

/** Renders an integer with locale-aware grouping. */
export function formatCount(value: number): string {
  if (!Number.isFinite(value)) return '—';
  return new Intl.NumberFormat().format(Math.round(value));
}

/** Renders a whole percentage. */
export function formatPercent(value: number): string {
  if (!Number.isFinite(value)) return '—';
  return `${Math.round(value)}%`;
}

/** Renders a ratio of two counts as a percentage, guarding division by zero. */
export function formatRatio(part: number, whole: number): string {
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0) return '—';
  return formatPercent((part / whole) * 100);
}

function parseTimestamp(iso: string | null | undefined): Date | null {
  if (!iso) return null;
  // Timestamps are RFC 3339 from the API; `Date` parses them without any manual
  // string handling, and an unparseable value degrades to a dash.
  const parsed = new Date(iso);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

/**
 * Renders an absolute timestamp in the viewer's local zone.
 *
 * The API always speaks UTC; only the presentation is localised.
 */
export function formatDateTime(iso: string | null | undefined): string {
  const parsed = parseTimestamp(iso);
  if (!parsed) return '—';
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: 'medium',
    timeStyle: 'medium',
  }).format(parsed);
}

/** Renders a compact date, for table columns where seconds add no value. */
export function formatDate(iso: string | null | undefined): string {
  const parsed = parseTimestamp(iso);
  if (!parsed) return '—';
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' }).format(parsed);
}

const RELATIVE_STEPS: readonly (readonly [Intl.RelativeTimeFormatUnit, number])[] = [
  ['second', 60],
  ['minute', 60],
  ['hour', 24],
  ['day', 7],
  ['week', 4.345],
  ['month', 12],
  ['year', Number.POSITIVE_INFINITY],
];

/** Renders a timestamp relative to now, for example `3 minutes ago`. */
export function formatRelativeTime(iso: string | null | undefined, now = new Date()): string {
  const parsed = parseTimestamp(iso);
  if (!parsed) return '—';
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto' });
  let delta = (parsed.getTime() - now.getTime()) / 1000;
  for (const [unit, span] of RELATIVE_STEPS) {
    if (Math.abs(delta) < span) {
      return formatter.format(Math.round(delta), unit);
    }
    delta /= span;
  }
  return formatter.format(Math.round(delta), 'year');
}

/** Renders a duration in seconds as a short human string. */
export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '—';
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `${days}d ${hours % 24}h`;
}

/**
 * Splits an object key into its logical prefix segments.
 *
 * Record Store stores no directories: these segments exist only because a delimiter was
 * applied to the key, which is what makes breadcrumb navigation possible.
 */
export function keySegments(prefix: string): readonly string[] {
  return prefix.split('/').filter((segment) => segment.length > 0);
}

/** Returns the final segment of a key, used as a display name. */
export function keyBasename(key: string): string {
  const segments = keySegments(key);
  return segments.length > 0 ? (segments[segments.length - 1] as string) : key;
}

/** Truncates the middle of a long identifier so both ends stay readable. */
export function shortenIdentifier(value: string, keep = 8): string {
  if (value.length <= keep * 2 + 1) return value;
  return `${value.slice(0, keep)}…${value.slice(-keep)}`;
}
