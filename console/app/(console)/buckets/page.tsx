import type { Metadata } from 'next';

import { BucketsScreen } from '@/features/buckets/buckets-screen';

export const metadata: Metadata = { title: 'Buckets' };

export default function Page() {
  return <BucketsScreen />;
}
