import type { Metadata } from 'next';

import { WebhooksScreen } from '@/features/webhooks/webhooks-screen';

export const metadata: Metadata = { title: 'Webhooks' };

export default function Page() {
  return <WebhooksScreen />;
}
