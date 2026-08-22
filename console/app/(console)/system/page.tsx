import type { Metadata } from 'next';

import { HealthScreen } from '@/features/system/health-screen';

export const metadata: Metadata = { title: 'System health' };

export default function Page() {
  return <HealthScreen />;
}
