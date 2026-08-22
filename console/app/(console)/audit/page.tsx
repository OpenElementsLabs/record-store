import type { Metadata } from 'next';

import { AuditScreen } from '@/features/audit/audit-screen';

export const metadata: Metadata = { title: 'Audit log' };

export default function Page() {
  return <AuditScreen />;
}
