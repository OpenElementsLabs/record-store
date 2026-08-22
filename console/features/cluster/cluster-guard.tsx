'use client';

import Link from 'next/link';

import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import { useClusterEnabled } from '@/features/system/deployment';

/**
 * Guards cluster-only screens.
 *
 * Navigating to a cluster route on a standalone deployment explains the
 * situation instead of erroring, because the route is legitimate — it just does
 * not apply to this deployment.
 */
export function ClusterGuard({ children }: { readonly children: React.ReactNode }) {
  const enabled = useClusterEnabled();
  if (enabled) return <>{children}</>;
  return (
    <Card>
      <CardContent className="flex flex-col items-center gap-3 px-6 py-12 text-center">
        <p className="text-sm font-medium text-ink">Cluster features are not enabled</p>
        <p className="max-w-md text-sm text-ink-muted">
          This OES deployment is running standalone. Nodes, replication, repair, and rebalancing
          exist only when the server runs in cluster mode.
        </p>
        <Button asChild size="sm">
          <Link href="/">Back to overview</Link>
        </Button>
      </CardContent>
    </Card>
  );
}
