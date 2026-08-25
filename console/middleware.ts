import { NextResponse, type NextRequest } from 'next/server';

/**
 * The console's own policy.
 *
 * A fresh nonce lets the framework tag its own bootstrap scripts, so the policy
 * never has to allow inline scripting in general. Styles remain inline-capable
 * because the framework injects critical CSS, which is a far smaller risk than
 * permitting arbitrary script.
 */
function consolePolicy(nonce: string): string {
  return [
    "default-src 'self'",
    `script-src 'self' 'nonce-${nonce}' 'strict-dynamic'`,
    "style-src 'self' 'unsafe-inline'",
    // `blob:` covers object URLs the console creates itself. Stored bytes are
    // never turned into one: they are loaded from a same-origin route so the
    // response's own policy applies to them.
    "img-src 'self' data: blob:",
    "media-src 'self'",
    "font-src 'self'",
    // The console talks only to its own origin; the management API is reached
    // through this server, never directly from the browser.
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'self'",
    "frame-ancestors 'none'",
    // A PDF preview is framed from a same-origin bytes route whose own response
    // is sandboxed into an opaque origin.
    "frame-src 'self'",
    'upgrade-insecure-requests',
  ].join('; ');
}

/**
 * The public share page's policy.
 *
 * Written separately rather than inherited, because the two pages have genuinely
 * different needs. This one is opened by strangers, shows untrusted stored
 * content, and submits exactly one form to exactly one place — so it allows
 * nothing beyond what those require, and in particular allows no form
 * submission, no framing, and no connection anywhere but back here.
 */
function sharePolicy(nonce: string): string {
  return [
    "default-src 'self'",
    `script-src 'self' 'nonce-${nonce}' 'strict-dynamic'`,
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data:",
    "media-src 'self'",
    "font-src 'self'",
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    // The password form is posted with `fetch`, so no navigation-style
    // submission is needed at all.
    "form-action 'none'",
    // A share page must never be framed: a page that can be framed can be
    // clickjacked into revealing what it shows.
    "frame-ancestors 'none'",
    // Only the sandboxed bytes route, for the PDF viewer.
    "frame-src 'self'",
    'upgrade-insecure-requests',
  ].join('; ');
}

export function middleware(request: NextRequest): NextResponse {
  const nonce = Buffer.from(crypto.randomUUID()).toString('base64');
  // Read from the URL rather than `nextUrl` so the policy decision depends on
  // the request itself and stays testable against a plain `Request`.
  const isSharePage = new URL(request.url).pathname.startsWith('/s/');
  const policy = isSharePage ? sharePolicy(nonce) : consolePolicy(nonce);

  const headers = new Headers(request.headers);
  headers.set('x-nonce', nonce);
  headers.set('content-security-policy', policy);

  const response = NextResponse.next({ request: { headers } });
  response.headers.set('content-security-policy', policy);
  // A shared file is not a search result and not a referrer source.
  if (isSharePage) {
    response.headers.set('referrer-policy', 'no-referrer');
    response.headers.set('x-robots-tag', 'noindex, nofollow, noarchive');
  }
  return response;
}

export const config = {
  matcher: [
    // Documents only. Static assets need no policy header, and byte-serving
    // routes must not pass through here at all: this middleware re-issues the
    // request, which caps how large a body may be, and an object streaming
    // through would be silently truncated at that cap. That rules out `/api`
    // and the share content and unlock routes, which are excluded by their
    // trailing path segments below. The share *page* is a document and does
    // pass through. Embeds are not served by this application at all — they
    // live on the storage endpoint. API responses are data rather than
    // documents, so they get their own policy from `next.config.ts` instead of
    // a nonce from here.
    '/((?!api/|s/[^/]+/|_next/static|_next/image|favicon.ico).*)',
  ],
};
