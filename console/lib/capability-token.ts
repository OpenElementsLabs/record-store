/**
 * The shape of a public capability token.
 *
 * Validated at the edge so a hostile path segment never reaches an upstream URL
 * and an obviously invalid guess costs a length check rather than a lookup. This
 * mirrors the backend's own parser, which remains the authority: a token that
 * passes here is still resolved, checked, and rate-limited there.
 */
export const CAPABILITY_TOKEN_LENGTH = 43;

const TOKEN_PATTERN = /^[A-Za-z0-9_-]+$/;

export function isCapabilityToken(value: string): boolean {
  return value.length === CAPABILITY_TOKEN_LENGTH && TOKEN_PATTERN.test(value);
}
