/** Lifetimes the backend accepts, which bounds them between a minute and a day. */
export const TEMPORARY_LIFETIMES = [
  { label: '15 minutes', seconds: 15 * 60 },
  { label: '1 hour', seconds: 60 * 60 },
  { label: '8 hours', seconds: 8 * 60 * 60 },
  { label: '24 hours', seconds: 24 * 60 * 60 },
] as const;

/**
 * How long a credential has left, or `null` if it does not expire.
 *
 * A credential whose expiry has passed returns zero rather than a negative
 * number, so the UI can say "expired" instead of counting backwards.
 */
export function secondsRemaining(expiresAt: string | null, now: Date): number | null {
  if (expiresAt === null) return null;
  const expiry = Date.parse(expiresAt);
  if (Number.isNaN(expiry)) return null;
  return Math.max(0, Math.round((expiry - now.getTime()) / 1000));
}
