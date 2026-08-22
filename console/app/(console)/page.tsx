import type { Metadata } from 'next';

import { OverviewScreen } from '@/features/overview/overview-screen';

export const metadata: Metadata = { title: 'Overview' };

export default function Page() {
  return <OverviewScreen />;
}
