/**
 * Parsing and presenting byte limits without losing the backend's exact value.
 *
 * A quota is stored as an exact byte count. Editing it through a unit selector
 * must therefore round-trip: a limit the operator never touched has to be sent
 * back byte-identical, and one they typed as "5 GB" has to become exactly
 * 5 * 1000^3 rather than a value that drifts through a float.
 */

/** Decimal units, matching how the console formats sizes elsewhere. */
export const BYTE_UNITS = ['B', 'kB', 'MB', 'GB', 'TB', 'PB'] as const;

export type ByteUnit = (typeof BYTE_UNITS)[number];

const MULTIPLIER: Record<ByteUnit, number> = {
  B: 1,
  kB: 1000,
  MB: 1000 ** 2,
  GB: 1000 ** 3,
  TB: 1000 ** 4,
  PB: 1000 ** 5,
};

/** The largest unit that represents `bytes` without a remainder. */
export function exactUnit(bytes: number): ByteUnit {
  if (!Number.isFinite(bytes) || bytes <= 0) return 'B';
  let chosen: ByteUnit = 'B';
  for (const unit of BYTE_UNITS) {
    if (bytes % MULTIPLIER[unit] === 0) chosen = unit;
  }
  return chosen;
}

/**
 * Splits a byte count into the largest unit that divides it exactly.
 *
 * Choosing an exact divisor rather than the most human-readable unit is what
 * keeps editing lossless: 1_500_000_000 shows as 1500 MB, not 1.5 GB, because
 * re-entering 1.5 GB would be a different number of bytes than was stored.
 */
export function splitBytes(bytes: number): { readonly value: number; readonly unit: ByteUnit } {
  const unit = exactUnit(bytes);
  return { value: bytes / MULTIPLIER[unit], unit };
}

/** Combines a value and unit into an exact byte count, or `null` if unusable. */
export function toBytes(value: string, unit: ByteUnit): number | null {
  const trimmed = value.trim();
  if (trimmed.length === 0) return null;
  // Only whole amounts are accepted, so no fractional unit can round.
  if (!/^\d+$/.test(trimmed)) return null;
  const amount = Number(trimmed);
  if (!Number.isSafeInteger(amount)) return null;
  const bytes = amount * MULTIPLIER[unit];
  return Number.isSafeInteger(bytes) ? bytes : null;
}
