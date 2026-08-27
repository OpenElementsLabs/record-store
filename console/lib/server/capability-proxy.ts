/**
 * The forwarding boundary for public share traffic.
 *
 * This is deliberately a *different* boundary from the management proxy, and
 * the difference is the point: nothing here attaches a session, a bearer token,
 * or any other credential. A share token authorizes itself, and adding an
 * ambient credential to these requests would turn a narrow read capability into
 * whatever the console's own session can do.
 *
 * Only shares come through here. An embed is served by the storage endpoint
 * directly, because it is loaded by somebody else's page and has no business
 * reaching an administrative console; a share is a page Record Store itself renders, so
 * its bytes stay same-origin with the viewer that shows them.
 *
 * It also forwards more than the management proxy does. Range requests, range
 * responses, and validators all have to survive the hop, because a video that
 * cannot seek is a delivery failure that looks like a storage failure.
 */

import { managementApiUrl } from './config';

/** Request headers that are meaningful to a share route. */
const FORWARDED_REQUEST_HEADERS = [
  'accept',
  'accept-encoding',
  'content-type',
  // Range and its validators are what make media seeking work at all.
  'range',
  'if-range',
  'if-none-match',
  'if-modified-since',
  // The embed origin check reads this, and it must arrive unaltered.
  'origin',
  'x-request-id',
  // Proof that a share password was already entered.
  'x-record-store-share-ticket',
];

/**
 * Response headers copied back to the caller.
 *
 * An allowlist rather than a passthrough, so a header Record Store gains later cannot
 * start reaching the public without anyone deciding that it should.
 */
const FORWARDED_RESPONSE_HEADERS = [
  'content-type',
  'content-length',
  'content-disposition',
  'content-range',
  'accept-ranges',
  'etag',
  'last-modified',
  'cache-control',
  'vary',
  'retry-after',
  'x-content-type-options',
  'content-security-policy',
  'access-control-allow-origin',
  'access-control-allow-methods',
  'access-control-allow-headers',
  'access-control-max-age',
  'cross-origin-resource-policy',
  'x-request-id',
];

/** How the client is identified to the backend's abuse controls. */
function clientAddress(request: Request): string | null {
  const forwarded = request.headers.get('x-forwarded-for')?.split(',', 1)[0]?.trim();
  if (forwarded && forwarded.length <= 64) return forwarded;
  const real = request.headers.get('x-real-ip')?.trim();
  if (real && real.length <= 64) return real;
  return null;
}

/**
 * Forwards one public share request to the management API.
 *
 * `path` is built by the caller from a validated token, never from raw user
 * input, so nothing here can be steered at another route.
 */
export async function forwardCapabilityRequest(
  request: Request,
  path: string,
  options: { readonly search?: string } = {},
): Promise<Response> {
  const target = `${managementApiUrl()}${path}${options.search ?? ''}`;

  const headers = new Headers();
  for (const name of FORWARDED_REQUEST_HEADERS) {
    const value = request.headers.get(name);
    if (value) headers.set(name, value);
  }
  // Set rather than forwarded: the backend partitions its rate limiters by this
  // value, and a client that could choose its own would be choosing its own
  // allowance. Whatever the deployment's own proxy determined wins, and when
  // there is no proxy the header is absent and the backend falls back to the
  // socket address.
  const client = clientAddress(request);
  if (client) {
    headers.set('x-forwarded-for', client);
  } else {
    headers.delete('x-forwarded-for');
  }

  const hasBody = request.method !== 'GET' && request.method !== 'HEAD';
  let upstream: Response;
  try {
    upstream = await fetch(target, {
      method: request.method,
      headers,
      ...(hasBody ? { body: request.body, duplex: 'half' } : {}),
      redirect: 'manual',
      cache: 'no-store',
    } as RequestInit);
  } catch {
    return new Response(
      JSON.stringify({
        error: { code: 'UNAVAILABLE', message: 'This link is temporarily unavailable.' },
      }),
      { status: 503, headers: { 'content-type': 'application/json', 'cache-control': 'no-store' } },
    );
  }

  const responseHeaders = new Headers();
  for (const name of FORWARDED_RESPONSE_HEADERS) {
    const value = upstream.headers.get(name);
    if (value) responseHeaders.set(name, value);
  }
  if (!responseHeaders.has('cache-control')) {
    responseHeaders.set('cache-control', 'no-store');
  }

  // The body is streamed rather than buffered, so a multi-gigabyte object costs
  // this process a socket rather than its memory — and so `content-length` stays
  // true, which is what keeps a range response a range response.
  return new Response(upstream.body, {
    status: upstream.status,
    headers: responseHeaders,
  });
}

/** Reads a share's public descriptor for server-side rendering. */
export async function readShareDescriptor(
  token: string,
  ticket: string | null,
): Promise<{ readonly status: number; readonly body: unknown }> {
  const headers = new Headers({ accept: 'application/json' });
  if (ticket) headers.set('x-record-store-share-ticket', ticket);
  try {
    const response = await fetch(`${managementApiUrl()}/s/${encodeURIComponent(token)}`, {
      headers,
      cache: 'no-store',
    });
    return { status: response.status, body: await response.json().catch(() => null) };
  } catch {
    return { status: 503, body: null };
  }
}
