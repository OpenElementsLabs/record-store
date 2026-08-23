import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { DurabilityScreen } from '@/features/cluster/durability-screen';

export const metadata: Metadata = { title: 'Durability' };

export default function Page() {
  return (
    <ClusterGuard>
      <DurabilityScreen />
    </ClusterGuard>
  );
}
