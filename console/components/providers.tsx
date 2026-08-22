'use client';

import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { useRouter } from 'next/navigation';
import * as React from 'react';
import { Toaster } from 'sonner';

import { ApiError } from '@/lib/api/error';

/**
 * Creates the shared query client.
 *
 * Defaults are chosen for an operational console: data stays usable while it
 * revalidates, failures that cannot be fixed by retrying are not retried, and a
 * lost session sends the operator to sign in exactly once rather than looping.
 */
function createQueryClient(onUnauthorized: () => void): QueryClient {
  const handle = (error: unknown) => {
    if (error instanceof ApiError && error.isUnauthorized) onUnauthorized();
  };
  return new QueryClient({
    queryCache: new QueryCache({ onError: handle }),
    mutationCache: new MutationCache({ onError: handle }),
    defaultOptions: {
      queries: {
        staleTime: 30_000,
        gcTime: 5 * 60_000,
        // Keeping the previous page visible avoids blanking a table on refetch.
        placeholderData: <T,>(previous: T) => previous,
        retry: (attempt, error) => {
          if (error instanceof ApiError) {
            // Authorisation, missing resources, and rejected input will not
            // become valid by asking again.
            if (['unauthorized', 'forbidden', 'not-found', 'invalid'].includes(error.kind)) {
              return false;
            }
          }
          return attempt < 2;
        },
        refetchOnWindowFocus: true,
        // Polling stops while the tab is in the background so an idle console
        // does not keep loading the management API.
        refetchIntervalInBackground: false,
      },
      mutations: { retry: false },
    },
  });
}

export function Providers({ children }: { readonly children: React.ReactNode }) {
  const router = useRouter();

  const onUnauthorized = React.useCallback(() => {
    if (typeof window === 'undefined') return;
    // Already signing in: redirecting again would loop.
    if (window.location.pathname === '/login') return;
    const next = encodeURIComponent(window.location.pathname + window.location.search);
    router.replace(`/login?next=${next}`);
  }, [router]);

  const [client] = React.useState(() => createQueryClient(onUnauthorized));

  return (
    <QueryClientProvider client={client}>
      {children}
      <Toaster position="bottom-right" closeButton richColors />
    </QueryClientProvider>
  );
}
