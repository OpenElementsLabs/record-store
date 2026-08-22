/**
 * Management session handling.
 *
 * The console holds the management credential in an HTTP-only cookie. Script on
 * the page cannot read it, it is never placed in `localStorage`, and it is
 * attached to upstream requests only by this server.
 */

import { cookies } from 'next/headers';

import { SESSION_COOKIE, secureCookies } from './config';

/** Reads the management credential for the current request, if signed in. */
export async function readSessionToken(): Promise<string | null> {
  const store = await cookies();
  const value = store.get(SESSION_COOKIE)?.value;
  return value && value.length > 0 ? value : null;
}

/** Cookie attributes used for both setting and clearing the session. */
export function sessionCookieOptions(maxAgeSeconds: number) {
  return {
    httpOnly: true,
    // `Strict` keeps the credential off cross-site navigations entirely, which
    // is the primary defence against cross-site request forgery here.
    sameSite: 'strict' as const,
    secure: secureCookies(),
    path: '/',
    maxAge: maxAgeSeconds,
  };
}
