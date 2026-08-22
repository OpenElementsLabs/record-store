import type { Metadata } from 'next';

import { MetricsScreen } from '@/features/system/metrics-screen';

export const metadata: Metadata = { title: 'Metrics' };

export default function Page() {
  return <MetricsScreen />;
}
