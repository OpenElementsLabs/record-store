import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { ClusterScreen } from '@/features/cluster/cluster-screen';

export const metadata: Metadata = { title: 'Cluster' };

export default function Page() {
  return (
    <ClusterGuard>
      <ClusterScreen />
    </ClusterGuard>
  );
}
