import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { NodesScreen } from '@/features/cluster/nodes-screen';

export const metadata: Metadata = { title: 'Nodes' };

export default function Page() {
  return (
    <ClusterGuard>
      <NodesScreen />
    </ClusterGuard>
  );
}
