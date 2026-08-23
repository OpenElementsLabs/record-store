import { describe, expect, it } from 'vitest';

import { describeLifecycleRule } from './lifecycle-summary';
import type { LifecycleRule } from '@/types/api';

function rule(overrides: Partial<LifecycleRule> = {}): LifecycleRule {
  return {
    id: 'r1',
    bucket_id: 'b1',
    prefix: '',
    enabled: true,
    expiration: null,
    noncurrent_version_expiration: null,
    created_at: '2026-08-01T00:00:00Z',
    updated_at: '2026-08-01T00:00:00Z',
    ...overrides,
  };
}

describe('describeLifecycleRule', () => {
  it('names the prefix a rule is scoped to', () => {
    expect(describeLifecycleRule(rule({ prefix: 'backups/', expiration: 90 }))).toBe(
      'Objects under backups/ are deleted 90 days after creation.',
    );
  });

  it('says when a rule covers the whole bucket', () => {
    // An empty prefix is easy to misread as "no rule"; it means everything.
    expect(describeLifecycleRule(rule({ expiration: 30 }))).toBe(
      'All objects in this bucket are deleted 30 days after creation.',
    );
  });

  it('joins both expiries into one sentence', () => {
    expect(
      describeLifecycleRule(
        rule({ prefix: 'logs/', expiration: 90, noncurrent_version_expiration: 30 }),
      ),
    ).toBe(
      'Objects under logs/ are deleted 90 days after creation, and have their non-current versions deleted 30 days after being replaced.',
    );
  });

  it('describes a non-current-only rule without implying current objects expire', () => {
    const summary = describeLifecycleRule(rule({ noncurrent_version_expiration: 7 }));
    expect(summary).toBe(
      'All objects in this bucket have their non-current versions deleted 7 days after being replaced.',
    );
    expect(summary).not.toMatch(/after creation/);
  });

  it('reads naturally for a single day', () => {
    expect(describeLifecycleRule(rule({ expiration: 1 }))).toContain('deleted 1 day after');
  });

  it('states plainly when a rule is disabled', () => {
    // A disabled rule that reads like an active one is how data survives that
    // an operator expected to be gone, or vice versa.
    expect(describeLifecycleRule(rule({ expiration: 30, enabled: false }))).toContain(
      'This rule is currently disabled.',
    );
  });

  it('does not invent an action for a rule that sets none', () => {
    expect(describeLifecycleRule(rule())).toBe(
      'All objects in this bucket are not affected: this rule sets no expiry.',
    );
  });
});
