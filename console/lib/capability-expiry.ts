/**
 * Expiry choices for share and embed links.
 *
 * Offered as a small set of durations rather than a date picker because the
 * decision an operator is actually making is "how long should this be reachable
 * for", and a calendar makes them translate that into a date themselves. The
 * exact instant is computed from the moment of creation, which is also what the
 * backend validates against.
 */
export type ExpiryChoice = {
  readonly id: string;
  readonly label: string;
  /** Null means the link never expires. */
  readonly days: number | null;
};

export const EXPIRY_CHOICES: readonly ExpiryChoice[] = [
  { id: '1d', label: '1 day', days: 1 },
  { id: '7d', label: '7 days', days: 7 },
  { id: '30d', label: '30 days', days: 30 },
  { id: '90d', label: '90 days', days: 90 },
  { id: '365d', label: '1 year', days: 365 },
  { id: 'never', label: 'Never expires', days: null },
];

/** The default: long enough to be useful, short enough to be forgotten safely. */
export const DEFAULT_EXPIRY_ID = '7d';

/**
 * Narrows the offered choices to what the deployment will actually accept.
 *
 * A dialog that offers a year to a deployment capped at thirty days produces a
 * rejection the operator could not have predicted, so the ceiling is applied to
 * the options rather than to the error message.
 */
export function availableExpiryChoices(
  maximumLifetimeDays: number | null,
  requireExpiration: boolean,
): readonly ExpiryChoice[] {
  return EXPIRY_CHOICES.filter((choice) => {
    if (choice.days === null) return !requireExpiration;
    if (maximumLifetimeDays === null) return true;
    return choice.days <= maximumLifetimeDays;
  });
}

/** Converts a choice into the RFC 3339 instant the API expects. */
export function expiryInstant(choice: ExpiryChoice, now: Date = new Date()): string | null {
  if (choice.days === null) return null;
  return new Date(now.getTime() + choice.days * 24 * 60 * 60 * 1000).toISOString();
}

/** Finds a choice by identifier, falling back to the first available one. */
export function resolveExpiryChoice(
  choices: readonly ExpiryChoice[],
  id: string,
): ExpiryChoice | null {
  return choices.find((choice) => choice.id === id) ?? choices[0] ?? null;
}
