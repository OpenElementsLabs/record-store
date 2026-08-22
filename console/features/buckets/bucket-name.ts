/**
 * Client-side bucket name checks.
 *
 * These mirror the common cases so the operator gets immediate feedback, and
 * they deliberately stop short of reimplementing the full rule set: the backend
 * remains authoritative and its rejection is always shown.
 */
export function validateBucketName(name: string): string | null {
  if (name.length === 0) return 'Enter a bucket name.';
  if (name.length < 3 || name.length > 63) {
    return 'A bucket name must be between 3 and 63 characters.';
  }
  if (!/^[a-z0-9][a-z0-9.-]*[a-z0-9]$/.test(name)) {
    return 'Use lowercase letters, digits, hyphens, and dots, starting and ending with a letter or digit.';
  }
  if (name.includes('..') || name.includes('.-') || name.includes('-.')) {
    return 'Dots and hyphens cannot be adjacent.';
  }
  if (/^\d+\.\d+\.\d+\.\d+$/.test(name)) {
    return 'A bucket name must not look like an IP address.';
  }
  return null;
}
