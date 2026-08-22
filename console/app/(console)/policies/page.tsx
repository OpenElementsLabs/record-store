import type { Metadata } from 'next';

import { PoliciesScreen } from '@/features/access/policies-screen';

export const metadata: Metadata = { title: 'Policies' };

export default function Page() {
  return <PoliciesScreen />;
}
