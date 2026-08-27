import type { NextConfig } from 'next';

/**
 * Security headers applied to every console response.
 *
 * The content security policy is nonce based: `middleware.ts` mints a nonce per
 * request so Next.js can tag its own bootstrap scripts without the policy
 * having to allow inline scripting in general.
 */
const securityHeaders = [
  { key: 'X-Content-Type-Options', value: 'nosniff' },
  { key: 'X-Frame-Options', value: 'DENY' },
  { key: 'Referrer-Policy', value: 'no-referrer' },
  { key: 'Cross-Origin-Opener-Policy', value: 'same-origin' },
  {
    key: 'Permissions-Policy',
    value: 'camera=(), microphone=(), geolocation=(), payment=()',
  },
];

/**
 * The policy for API responses, which are payloads rather than pages.
 *
 * `sandbox` is deliberately absent: object downloads are served from this path
 * with `Content-Disposition: attachment`, and sandboxing a navigation response
 * can suppress the download the operator asked for.
 */
const apiPolicy = "default-src 'none'; frame-ancestors 'none'; base-uri 'none'";

/**
 * The policy for stored bytes served for viewing.
 *
 * `sandbox` puts the response in an opaque origin, so a PDF still renders in the
 * browser's own viewer while anything the document tries to do — script, form
 * submission, top-level navigation — has no origin to do it against.
 * `allow-downloads` is the one capability kept, because a viewer's own save
 * button is a legitimate thing for a reader to press.
 *
 * `frame-ancestors 'self'` is what lets the console and the share page mount the
 * viewer, and `X-Frame-Options` has to be relaxed to `SAMEORIGIN` alongside it
 * because the blanket `DENY` above would otherwise win in browsers that honour
 * both.
 */
const viewerPolicy =
  "sandbox allow-downloads; default-src 'none'; frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

const viewerHeaders = [
  { key: 'Content-Security-Policy', value: viewerPolicy },
  { key: 'X-Frame-Options', value: 'SAMEORIGIN' },
  { key: 'X-Content-Type-Options', value: 'nosniff' },
];

const nextConfig: NextConfig = {
  reactStrictMode: true,
  poweredByHeader: false,
  // The console is deployed as a container image alongside the Record Store server.
  output: 'standalone',
  async headers() {
    return [
      { source: '/:path*', headers: securityHeaders },
      // API responses are data, never documents, so they are not routed through
      // the nonce middleware. They still carry a policy: if one is ever opened
      // directly, it may load nothing at all.
      { source: '/api/:path*', headers: [{ key: 'Content-Security-Policy', value: apiPolicy }] },
      // Ordered after the general rules so these override them for the two
      // paths that serve stored bytes for viewing. Both are same-origin frames
      // in a page this console controls.
      {
        source: '/api/record-store/v1/buckets/:bucket/object-preview/:path*',
        headers: viewerHeaders,
      },
      { source: '/s/:token/content', headers: viewerHeaders },
      // There is deliberately no rule for embeds here. Embed bytes are served by
      // the storage endpoint, not by this application, so a site loading an
      // asset never reaches the console at all.
    ];
  },
};

export default nextConfig;
