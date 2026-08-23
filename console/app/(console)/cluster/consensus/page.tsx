import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { ConsensusScreen } from '@/features/cluster/consensus-screen';

export const metadata: Metadata = { title: 'Consensus' };

export default function Page() {
  return (
    <ClusterGuard>
      <ConsensusScreen />
    </ClusterGuard>
  );
}
