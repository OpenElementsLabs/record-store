import type { Metadata } from 'next';

import { ServiceAccountDetail } from '@/features/access/service-account-detail';

export const metadata: Metadata = { title: 'Service account' };

export default async function Page({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return <ServiceAccountDetail accountId={id} />;
}
