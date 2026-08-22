/**
 * Server-only configuration.
 *
 * These values are read on the Next.js server and never shipped to the browser,
 * so the upstream API address stays a deployment concern rather than something
 * baked into a client bundle.
 */

/** Where the OES management API lives. */
export function managementApiUrl(): string {
  const configured = process.env.OES_API_URL ?? 'http://127.0.0.1:7601';
  return configured.replace(/\/+$/, '');
}

/** Name of the HTTP-only cookie carrying the management session. */
export const SESSION_COOKIE = 'oes_session';

/**
 * Whether cookies must be marked `Secure`.
 *
 * Enabled in production by default so a token is never sent over plain HTTP,
 * and overridable for deployments that terminate TLS elsewhere.
 */
export function secureCookies(): boolean {
  const override = process.env.OES_CONSOLE_SECURE_COOKIES;
  if (override === 'true') return true;
  if (override === 'false') return false;
  return process.env.NODE_ENV === 'production';
}
