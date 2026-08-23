import type { LifecycleRule } from '@/types/api';

/** Renders a day count as a phrase, keeping the singular readable. */
function days(count: number): string {
  return count === 1 ? '1 day' : `${count} days`;
}

/**
 * Describes what a lifecycle rule will actually do, in a sentence.
 *
 * A rule is a policy about deletion, and the list is where an operator decides
 * whether it is the policy they meant. Reading that off a prefix and two day
 * counts is error-prone, so the rule states itself.
 */
export function describeLifecycleRule(rule: LifecycleRule): string {
  const scope = rule.prefix ? `Objects under ${rule.prefix}` : 'All objects in this bucket';
  const clauses: string[] = [];
  if (rule.expiration !== null) {
    clauses.push(`are deleted ${days(rule.expiration)} after creation`);
  }
  if (rule.noncurrent_version_expiration !== null) {
    clauses.push(
      `have their non-current versions deleted ${days(rule.noncurrent_version_expiration)} after being replaced`,
    );
  }
  // A rule with no action is not something the backend accepts, but describing
  // it honestly beats rendering a sentence that trails off.
  if (clauses.length === 0) return `${scope} are not affected: this rule sets no expiry.`;

  const sentence = `${scope} ${clauses.join(', and ')}.`;
  return rule.enabled ? sentence : `${sentence} This rule is currently disabled.`;
}
