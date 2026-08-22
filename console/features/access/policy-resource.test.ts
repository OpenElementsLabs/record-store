import { describe, expect, it } from 'vitest';

import { isBroad, resourceProblem } from './policy-resource';

describe('resourceProblem', () => {
  it('accepts the shapes the backend accepts', () => {
    for (const resource of [
      'bucket:uploads',
      'bucket:uploads/*',
      'bucket:*',
      'bucket:logs/2026/*',
    ]) {
      expect(resourceProblem(resource)).toBeNull();
    }
  });

  it('rejects a pattern that names no bucket', () => {
    // A bare `*` reads as "everything", but the API has never accepted it.
    expect(resourceProblem('*')).toMatch(/must start with/);
    expect(resourceProblem('uploads/*')).toMatch(/must start with/);
  });

  it('rejects more than one wildcard', () => {
    expect(resourceProblem('bucket:*/*')).toMatch(/at most one/);
  });

  it('rejects a wildcard that is not the last character', () => {
    expect(resourceProblem('bucket:*/logs')).toMatch(/final character/);
    expect(resourceProblem('bucket:up*ads')).toMatch(/final character/);
  });

  it('rejects control characters', () => {
    expect(resourceProblem('bucket:up\u0007loads')).toMatch(/control characters/);
  });
});

describe('isBroad', () => {
  it('flags a pattern that can reach more than one bucket', () => {
    expect(isBroad('bucket:*')).toBe(true);
    expect(isBroad('bucket:log*')).toBe(true);
  });

  it('does not flag every object in a single named bucket', () => {
    // Wide, but scoped to one bucket, and the normal shape of a real policy.
    expect(isBroad('bucket:uploads/*')).toBe(false);
    expect(isBroad('bucket:uploads/reports/*')).toBe(false);
  });

  it('does not flag an exact resource', () => {
    expect(isBroad('bucket:uploads')).toBe(false);
  });
});
