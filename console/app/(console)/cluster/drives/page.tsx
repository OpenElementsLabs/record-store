import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { DrivesScreen } from '@/features/cluster/drives-screen';

export const metadata: Metadata = { title: 'Drives' };

export default function Page() {
  return (
    <ClusterGuard>
      <DrivesScreen />
    </ClusterGuard>
  );
}
