/**
 * The forwarding boundary between the browser and the Record Store management API.
 *
 * This layer is deliberately thin: it attaches the session credential, checks
 * the request origin, and streams bodies through. It contains no business rules,
 * because duplicating them here would create a second place for them to drift.
 */

import { managementApiUrl } from './config';
import { readSessionToken } from './session';

/** Methods that change state and therefore need an origin check. */
const MUTATING = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

/**
 * Headers forwarded upstream.
 *
 * An allowlist is used so browser-controlled headers cannot influence how the
 * management API authenticates or authorises the call. `Range` and its
 * validators are on the list because a preview of a large video or PDF is a
 * sequence of partial reads: without them the browser asks for a slice, receives
 * the whole object, and seeking silently stops working.
 */
const FORWARDED_REQUEST_HEADERS = [
  'content-type',
  'accept',
  'range',
  'if-range',
  'if-none-match',
  'if-modified-since',
  'x-request-id',
];

/**
 * Headers copied back to the browser.
 *
 * `Content-Range` and `Accept-Ranges` are what make a `206` mean anything: a
 * partial response without them is a truncated object as far as the browser is
 * concerned.
 */
const FORWARDED_RESPONSE_HEADERS = [
  'content-type',
  'content-length',
  'content-disposition',
  'content-range',
  'accept-ranges',
  'etag',
  'last-modified',
  'x-request-id',
  'cache-control',
  'x-content-type-options',
  'content-security-policy',
];

function jsonError(status: number, code: string, message: string): Response {
  return new Response(JSON.stringify({ error: { code, message, request_id: '' } }), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

/**
 * Rejects state-changing requests that did not originate from this console.
 *
 * `SameSite=Strict` already prevents the browser from attaching the session on
 * a cross-site request; this is a second, explicit check rather than a reliance
 * on cookie policy alone.
 */
function originAllowed(request: Request): boolean {
  const origin = request.headers.get('origin');
  if (!origin) {
    // Same-origin `fetch` from a browser always sends `Origin` for mutations.
    return false;
  }
  // Compare the complete external origin. A host-only comparison would accept
  // an HTTPS page presented as HTTP (or vice versa) behind a reverse proxy.
  const forwardedHost = request.headers.get('x-forwarded-host')?.split(',', 1)[0]?.trim();
  const host = forwardedHost || request.headers.get('host');
  if (!host) return false;
  const forwardedProtocol = request.headers.get('x-forwarded-proto')?.split(',', 1)[0]?.trim();
  const protocol = forwardedProtocol || new URL(request.url).protocol.slice(0, -1);
  if (protocol !== 'http' && protocol !== 'https') return false;
  try {
    return new URL(origin).origin === new URL(`${protocol}://${host}`).origin;
  } catch {
    return false;
  }
}

/** Forwards one request to the management API. */
export async function forwardToManagementApi(
  request: Request,
  segments: readonly string[],
): Promise<Response> {
  const token = await readSessionToken();
  if (!token) {
    return jsonError(401, 'UNAUTHORIZED', 'Sign in to continue.');
  }
  if (MUTATING.has(request.method) && !originAllowed(request)) {
    return jsonError(403, 'FORBIDDEN_ORIGIN', 'The request origin is not permitted.');
  }

  // Segments arrive already decoded; re-encoding each one keeps object keys with
  // slashes or spaces intact without letting them alter the upstream path shape.
  const path = segments.map(encodeURIComponent).join('/');
  const query = new URL(request.url).search;
  const target = `${managementApiUrl()}/api/${path}${query}`;

  const headers = new Headers();
  for (const name of FORWARDED_REQUEST_HEADERS) {
    const value = request.headers.get(name);
    if (value) headers.set(name, value);
  }
  headers.set('authorization', `Bearer ${token}`);

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
    return jsonError(
      503,
      'MANAGEMENT_API_UNREACHABLE',
      'The Record Store management API is unreachable.',
    );
  }

  const responseHeaders = new Headers();
  for (const name of FORWARDED_RESPONSE_HEADERS) {
    const value = upstream.headers.get(name);
    if (value) responseHeaders.set(name, value);
  }
  // Object payloads must never be cached by an intermediary on the way back.
  responseHeaders.set('cache-control', 'no-store');
  // A preview of an untrusted object must not be sniffed into something else,
  // whatever the upstream said.
  responseHeaders.set('x-content-type-options', 'nosniff');

  // The upstream body is streamed rather than buffered so object transfers are
  // bounded by the network, not by this process's memory.
  return new Response(upstream.body, {
    status: upstream.status,
    headers: responseHeaders,
  });
}

/** Validates a credential by asking the management API who it belongs to. */
export async function verifyCredential(
  token: string,
): Promise<{ ok: true; body: unknown } | { ok: false; status: number }> {
  try {
    const response = await fetch(`${managementApiUrl()}/api/v1/auth/session`, {
      headers: { authorization: `Bearer ${token}`, accept: 'application/json' },
      cache: 'no-store',
    });
    if (!response.ok) return { ok: false, status: response.status };
    return { ok: true, body: await response.json() };
  } catch {
    return { ok: false, status: 503 };
  }
}
