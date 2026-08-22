import type { Metadata } from 'next';

import { IntegrityScreen } from '@/features/integrity/integrity-screen';

export const metadata: Metadata = { title: 'Integrity' };

export default function Page() {
  return <IntegrityScreen />;
}
