import type { Metadata, Viewport } from 'next';
import { headers } from 'next/headers';

import { Providers } from '@/components/providers';

import './globals.css';

export const metadata: Metadata = {
  title: { default: 'OES Console', template: '%s · OES Console' },
  description: 'Administrative console for OES object storage.',
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
    <html lang="en" suppressHydrationWarning>
      <head>
        <script
          nonce={nonce}
          // Applies the stored theme before first paint so the page never
          // flashes the wrong palette.
          dangerouslySetInnerHTML={{
            __html: `(function(){try{var t=localStorage.getItem('oes-theme');var d=t==='dark'||(!t&&window.matchMedia('(prefers-color-scheme: dark)').matches);document.documentElement.classList.toggle('dark',d);}catch(e){}})();`,
          }}
        />
      </head>
      <body>
        <Providers>{children}</Providers>
      </body>
    </html>
  );
}
