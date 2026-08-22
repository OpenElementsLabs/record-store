/**
 * Type-safe reading of URL search parameters.
 *
 * Filters and pagination live in the URL so refresh, the back button, and shared
 * links all work. Malformed values fall back to a safe default rather than
 * throwing, because a URL is user-editable input.
 */

export type ReadonlyParams = {
  get(name: string): string | null;
};

/** Reads a bounded string, trimming and rejecting oversized values. */
export function readString(
  params: ReadonlyParams,
  name: string,
  fallback = '',
  maxLength = 1024,
): string {
  const raw = params.get(name);
  if (raw === null) return fallback;
  const trimmed = raw.trim();
  if (trimmed.length === 0 || trimmed.length > maxLength) return fallback;
  return trimmed;
}

/** Reads an optional string, returning `null` when absent or unusable. */
export function readOptionalString(
  params: ReadonlyParams,
  name: string,
  maxLength = 1024,
): string | null {
  const value = readString(params, name, '', maxLength);
  return value.length > 0 ? value : null;
}

/** Reads an integer, clamping it into range. */
export function readInt(
  params: ReadonlyParams,
  name: string,
  fallback: number,
  min: number,
  max: number,
): number {
  const raw = params.get(name);
  if (raw === null) return fallback;
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed)) return fallback;
  return Math.min(max, Math.max(min, parsed));
}

/** Reads a value constrained to a known set. */
export function readEnum<const T extends readonly string[]>(
  params: ReadonlyParams,
  name: string,
  allowed: T,
  fallback: T[number] | null = null,
): T[number] | null {
  const raw = params.get(name);
  if (raw === null) return fallback;
  return (allowed as readonly string[]).includes(raw) ? (raw as T[number]) : fallback;
}

/**
 * Reads an RFC 3339 timestamp.
 *
 * The value is validated by parsing rather than by pattern matching, so only
 * genuinely usable timestamps reach the API.
 */
export function readTimestamp(params: ReadonlyParams, name: string): string | null {
  const raw = params.get(name);
  if (raw === null || raw.length === 0) return null;
  const parsed = new Date(raw);
  return Number.isNaN(parsed.getTime()) ? null : parsed.toISOString();
}

/**
 * Builds a query string from a partial update.
 *
 * Empty values are removed so a cleared filter disappears from the URL instead
 * of lingering as an empty parameter.
 */
export function mergeSearch(
  current: ReadonlyParams & { forEach?: (fn: (value: string, key: string) => void) => void },
  updates: Readonly<Record<string, string | number | null | undefined>>,
): string {
  const next = new URLSearchParams();
  current.forEach?.((value, key) => {
    if (value.length > 0) next.set(key, value);
  });
  for (const [key, value] of Object.entries(updates)) {
    if (value === null || value === undefined || value === '') next.delete(key);
    else next.set(key, String(value));
  }
  const query = next.toString();
  return query.length > 0 ? `?${query}` : '';
}
