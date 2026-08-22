import { cookies } from 'next/headers';

import { SESSION_COOKIE } from '@/lib/server/config';
import { sessionCookieOptions } from '@/lib/server/session';

/** Clears the session cookie. */
export async function POST(): Promise<Response> {
  const store = await cookies();
  store.set(SESSION_COOKIE, '', sessionCookieOptions(0));
  return new Response(null, { status: 204, headers: { 'cache-control': 'no-store' } });
}

export const dynamic = 'force-dynamic';
