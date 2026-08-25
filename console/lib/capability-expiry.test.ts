import { describe, expect, it } from 'vitest';

import {
  availableExpiryChoices,
  expiryInstant,
  EXPIRY_CHOICES,
  resolveExpiryChoice,
} from './capability-expiry';

describe('capability expiry choices', () => {
  it('offers only lifetimes the deployment will accept', () => {
    // A dialog that offers a year to a deployment capped at thirty days
    // produces a rejection the operator could not have predicted.
    const choices = availableExpiryChoices(30, false);
    expect(choices.map((choice) => choice.id)).toEqual(['1d', '7d', '30d', 'never']);
  });

  it('drops the never-expires option when the deployment requires an expiry', () => {
    const choices = availableExpiryChoices(null, true);
    expect(choices.some((choice) => choice.days === null)).toBe(false);
  });

  it('keeps every option when the deployment sets no ceiling', () => {
    expect(availableExpiryChoices(null, false)).toHaveLength(EXPIRY_CHOICES.length);
  });

  it('computes the instant from the moment of creation', () => {
    const now = new Date('2026-03-01T12:00:00.000Z');
    const week = EXPIRY_CHOICES.find((choice) => choice.id === '7d');
    expect(week).toBeDefined();
    expect(expiryInstant(week!, now)).toBe('2026-03-08T12:00:00.000Z');
  });

  it('returns null for a link that never expires', () => {
    const never = EXPIRY_CHOICES.find((choice) => choice.days === null);
    expect(never).toBeDefined();
    expect(expiryInstant(never!)).toBeNull();
  });

  it('falls back to the first available option rather than returning nothing', () => {
    const choices = availableExpiryChoices(1, true);
    expect(resolveExpiryChoice(choices, 'never')?.id).toBe('1d');
    expect(resolveExpiryChoice([], 'anything')).toBeNull();
  });
});
