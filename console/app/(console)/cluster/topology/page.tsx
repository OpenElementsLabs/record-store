import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { TopologyScreen } from '@/features/cluster/topology-screen';

export const metadata: Metadata = { title: 'Topology' };

export default function Page() {
  return (
    <ClusterGuard>
      <TopologyScreen />
    </ClusterGuard>
  );
}
