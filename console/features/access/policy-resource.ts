/**
 * Client-side checks for policy resource patterns.
 *
 * These mirror what the backend enforces so the operator gets immediate
 * feedback instead of a rejected request. The backend remains authoritative
 * and its refusal is always shown.
 */

/**
 * Why a resource pattern is not a resource pattern.
 *
 * The rule mirrors what `oes-auth` enforces, so the editor refuses a pattern
 * the API would reject rather than letting the operator discover it from a 400.
 * The backend remains the authority; this only spares a round trip.
 */
export function resourceProblem(resource: string): string | null {
  if (!resource.startsWith('bucket:')) {
    return 'A resource must start with “bucket:”, for example bucket:uploads/*.';
  }
  const wildcards = resource.split('*').length - 1;
  if (wildcards > 1) return 'A resource may contain at most one “*”.';
  if (wildcards === 1 && !resource.endsWith('*')) {
    return 'A “*” is only allowed as the final character.';
  }
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(resource)) {
    return 'A resource must not contain control characters.';
  }
  return null;
}

/**
 * Whether a pattern can reach more than one bucket.
 *
 * A trailing wildcard is only cross-bucket while the part before it still names
 * no single bucket: `bucket:*` and `bucket:log*` span buckets, whereas
 * `bucket:uploads/*` is every object in exactly one.
 */
export function isBroad(resource: string): boolean {
  if (!resource.startsWith('bucket:') || !resource.endsWith('*')) return false;
  const stem = resource.slice('bucket:'.length, -1);
  return !stem.includes('/');
}
