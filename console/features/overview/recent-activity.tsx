'use client';

import { useQuery } from '@tanstack/react-query';
import Link from 'next/link';

import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardHeader, CardTitle } from '@/components/ui/card';
import { TableSkeleton } from '@/components/ui/skeleton';
import { useCapabilities } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { fetchStorageEvents } from '@/lib/api/observability';
import { formatBytes, formatRelativeTime } from '@/lib/format';

/**
 * Recent storage activity.
 *
 * These are storage events — what happened to data. The audit trail is a
 * separate feed answering who requested it, and the two are never merged.
 */
export function RecentActivity() {
  const capabilities = useCapabilities();
  const events = useQuery({
    queryKey: queryKeys.events('recent'),
    queryFn: ({ signal }) => fetchStorageEvents({ limit: 8 }, signal),
    enabled: capabilities.events,
    refetchInterval: 20_000,
  });

  if (!capabilities.events) return null;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Recent activity</CardTitle>
        <Button asChild size="sm" variant="ghost">
          <Link href="/events">View all</Link>
        </Button>
      </CardHeader>
      {events.isError ? (
        <ErrorState error={events.error} onRetry={() => void events.refetch()} />
      ) : events.isPending ? (
        <TableSkeleton rows={4} columns={3} />
      ) : events.data.events.length === 0 ? (
        <EmptyState
          title="No activity yet"
          description="Storage events appear here as objects and buckets change."
        />
      ) : (
        <ul className="divide-y divide-border">
          {events.data.events.map((event) => (
            <li key={event.id} className="flex items-center gap-3 px-4 py-2.5">
              <Badge tone="neutral" className="shrink-0 font-mono">
                {event.type}
              </Badge>
              <span className="min-w-0 flex-1 truncate text-sm text-ink">
                {event.object ? `${event.bucket}/${event.object}` : event.bucket}
              </span>
              {event.size !== null ? (
                <span className="shrink-0 text-xs tabular-nums text-ink-muted">
                  {formatBytes(event.size)}
                </span>
              ) : null}
              <time
                dateTime={event.time}
                className="shrink-0 text-xs text-ink-subtle"
                title={event.time}
              >
                {formatRelativeTime(event.time)}
              </time>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
