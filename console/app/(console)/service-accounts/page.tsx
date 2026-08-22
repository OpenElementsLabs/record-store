import type { Metadata } from 'next';

import { ServiceAccountsScreen } from '@/features/access/service-accounts-screen';

export const metadata: Metadata = { title: 'Service accounts' };

export default function Page() {
  return <ServiceAccountsScreen />;
}
