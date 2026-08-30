import type { Metadata } from 'next';

import { ClusterGuard } from '@/features/cluster/cluster-guard';
import { StorageClassesScreen } from '@/features/cluster/storage-classes-screen';

export const metadata: Metadata = { title: 'Storage classes' };

export default function Page() {
  return (
    <ClusterGuard>
      <StorageClassesScreen />
    </ClusterGuard>
  );
}
