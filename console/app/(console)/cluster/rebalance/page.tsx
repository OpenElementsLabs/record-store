import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { RebalanceScreen } from '@/features/cluster/rebalance-screen';

export const metadata: Metadata = { title: 'Rebalancing' };

export default function Page() {
  return (
    <ClusterGuard>
      <RebalanceScreen />
    </ClusterGuard>
  );
}
