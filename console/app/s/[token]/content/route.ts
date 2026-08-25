import { cookies } from 'next/headers';

import { isCapabilityToken } from '@/lib/capability-token';
import { SHARE_TICKET_COOKIE } from '@/lib/server/config';
import { forwardCapabilityRequest } from '@/lib/server/capability-proxy';

/**
 * Serves the bytes behind a share link.
 *
 * Kept apart from the share page itself so the two can carry different security
 * headers: the page is a document that runs the console's own script, and this
 * is untrusted stored content that must never be treated as one.
 *
 * An element load — an `<img>`, a media player's range request — cannot set a
 * header, so the unlock ticket travels as a path-scoped cookie and is promoted
 * to a header here. The ticket alone authorizes nothing: the backend re-reads
 * the share and re-checks revocation, expiry, permission, and budget on every
 * request it accompanies.
 */
type Context = { params: Promise<{ token: string }> };

async function handle(request: Request, context: Context): Promise<Response> {
  const { token } = await context.params;
  if (!isCapabilityToken(token)) {
    return new Response(null, { status: 404, headers: { 'cache-control': 'no-store' } });
  }
  const url = new URL(request.url);
  // Only the one parameter the backend understands is carried through, so the
  // query string cannot be used to steer anything else.
  const download = url.searchParams.get('download') === 'true' ? '?download=true' : '';

  const forwarded = new Request(request, {});
  const ticket =
    request.headers.get('x-oes-share-ticket') ??
    (await cookies()).get(SHARE_TICKET_COOKIE)?.value ??
    null;
  if (ticket) forwarded.headers.set('x-oes-share-ticket', ticket);

  return forwardCapabilityRequest(forwarded, `/s/${token}/content`, { search: download });
}

export const GET = handle;
export const HEAD = handle;

export const dynamic = 'force-dynamic';
