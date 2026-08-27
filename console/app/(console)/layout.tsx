import { redirect } from 'next/navigation';

import { AppShell } from '@/components/app-shell';
import { managementApiUrl } from '@/lib/server/config';
import { readSessionToken } from '@/lib/server/session';
import type { Session, SystemInfo } from '@/types/api';

/**
 * Loads the deployment description before rendering any screen.
 *
 * Resolving mode, capabilities, and role on the server means the first paint
 * already has the right navigation, and an expired session never renders a shell
 * the operator cannot use.
 */
async function loadDeployment(
  token: string,
): Promise<{ info: SystemInfo; session: Session } | 'unauthorized' | 'unreachable'> {
  const base = managementApiUrl();
  const headers = { authorization: `Bearer ${token}`, accept: 'application/json' };
  try {
    const [infoResponse, sessionResponse] = await Promise.all([
      fetch(`${base}/api/v1/system/info`, { headers, cache: 'no-store' }),
      fetch(`${base}/api/v1/auth/session`, { headers, cache: 'no-store' }),
    ]);
    if (sessionResponse.status === 401) return 'unauthorized';
    if (!infoResponse.ok || !sessionResponse.ok) return 'unreachable';
    return {
      info: (await infoResponse.json()) as SystemInfo,
      session: (await sessionResponse.json()) as Session,
    };
  } catch {
    return 'unreachable';
  }
}

export default async function ConsoleLayout({ children }: { children: React.ReactNode }) {
  const token = await readSessionToken();
  if (!token) redirect('/login');

  const deployment = await loadDeployment(token);
  if (deployment === 'unauthorized') redirect('/login');
  if (deployment === 'unreachable') {
    return (
      <main className="flex min-h-screen items-center justify-center px-4">
        <div className="max-w-md space-y-2 text-center" role="alert">
          <h1 className="text-lg font-semibold text-ink">Record Store is unreachable</h1>
          <p className="text-sm text-ink-muted">
            The console could not reach the Record Store management API. It will work again as soon
            as the API responds; no console state has been lost.
          </p>
        </div>
      </main>
    );
  }

  return <AppShell deployment={deployment}>{children}</AppShell>;
}
