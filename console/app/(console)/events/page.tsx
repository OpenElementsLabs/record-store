import type { Metadata } from 'next';

import { EventsScreen } from '@/features/events/events-screen';

export const metadata: Metadata = { title: 'Events' };

export default function Page() {
  return <EventsScreen />;
}
