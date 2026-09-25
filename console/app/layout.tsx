import { GeistMono } from 'geist/font/mono';
import '@fontsource-variable/manrope';
import '@fontsource-variable/public-sans';
import type { Metadata, Viewport } from 'next';
import { headers } from 'next/headers';

import { Providers } from '@/components/providers';

import './globals.css';

export const metadata: Metadata = {
  applicationName: 'Record Store',
  title: { default: 'Record Store Console', template: '%s · Record Store Console' },
  description: 'Administrative console for Record Store object storage.',
  robots: { index: false, follow: false },
};

export const viewport: Viewport = {
  width: 'device-width',
  initialScale: 1,
};

export default async function RootLayout({ children }: { children: React.ReactNode }) {
  // The nonce is minted per request by the middleware. Reading it here lets the
  // framework tag its own scripts so the policy never needs inline script.
  const nonce = (await headers()).get('x-nonce') ?? undefined;
  return (
    // All font files are bundled locally, including the monospace face used
    // for identifiers. No external font service is needed.
    <html lang="en" className={GeistMono.variable} suppressHydrationWarning>
      <head>
        <script
          nonce={nonce}
          // Applies the stored theme before first paint so the page never
          // flashes the wrong palette.
          dangerouslySetInnerHTML={{
            __html: `(function(){try{var t=localStorage.getItem('record-store-theme');var d=t==='dark'||(!t&&window.matchMedia('(prefers-color-scheme: dark)').matches);document.documentElement.classList.toggle('dark',d);}catch(e){}})();`,
          }}
        />
      </head>
      <body>
        <Providers>{children}</Providers>
      </body>
    </html>
  );
}
