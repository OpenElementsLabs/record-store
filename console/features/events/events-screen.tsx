'use client';

import { useQuery } from '@tanstack/react-query';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { usePathname, useRouter, useSearchParams } from 'next/navigation';

import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { TableSkeleton } from '@/components/ui/skeleton';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableShell,
} from '@/components/ui/table';
import { queryKeys } from '@/hooks/use-system';
import { fetchStorageEvents } from '@/lib/api/observability';
import { formatBytes, formatDateTime } from '@/lib/format';
import { mergeSearch, readEnum, readOptionalString } from '@/lib/search-params';
import type { StorageEventType } from '@/types/api';

const EVENT_TYPES = [
  'bucket.created',
  'bucket.deleted',
  'object.created',
  'object.updated',
  'object.deleted',
  'object.restored',
  'multipart.completed',
  'multipart.aborted',
] as const;

/**
 * Storage events: what happened to data.
 *
 * This is deliberately a different screen from the audit log. Merging the two
 * would conflate operational history with security history, which answer
 * different questions.
 */
export function EventsScreen() {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();

  const filters = {
    bucket: readOptionalString(params, 'bucket'),
    type: readEnum(params, 'type', EVENT_TYPES) as StorageEventType | null,
    prefix: readOptionalString(params, 'prefix'),
    afterTime: readOptionalString(params, 'after_time'),
    afterId: readOptionalString(params, 'after_id'),
  };

  const events = useQuery({
    queryKey: queryKeys.events(JSON.stringify(filters)),
    queryFn: ({ signal }) => fetchStorageEvents({ ...filters, limit: 50 }, signal),
    refetchInterval: 30_000,
  });

  function update(updates: Record<string, string | null>) {
    router.push(`${pathname}${mergeSearch(params, updates)}`);
  }

  return (
    <>
      <PageHeader
        title="Events"
        description="Storage events recorded as buckets and objects change. Webhooks deliver these same events."
      />

      <Card>
        <form
          className="grid gap-3 border-b border-border p-3 sm:grid-cols-2 lg:grid-cols-4"
          onSubmit={(event) => {
            event.preventDefault();
            const form = new FormData(event.currentTarget);
            update({
              bucket: (form.get('bucket') as string) || null,
              type: (form.get('type') as string) || null,
              prefix: (form.get('prefix') as string) || null,
              after_time: null,
              after_id: null,
            });
          }}
        >
          <div className="space-y-1.5">
            <label htmlFor="event-bucket" className="type-label">
              Bucket
            </label>
            <Input
              id="event-bucket"
              name="bucket"
              defaultValue={filters.bucket ?? ''}
              autoComplete="off"
            />
          </div>
          <div className="space-y-1.5">
            <label htmlFor="event-type" className="type-label">
              Type
            </label>
            <select
              id="event-type"
              name="type"
              defaultValue={filters.type ?? ''}
              className="h-9 w-full rounded-[--radius-control] border border-border-strong bg-surface px-2 type-body"
            >
              <option value="">Any</option>
              {EVENT_TYPES.map((type) => (
                <option key={type} value={type}>
                  {type}
                </option>
              ))}
            </select>
          </div>
          <div className="space-y-1.5">
            <label htmlFor="event-prefix" className="type-label">
              Key prefix
            </label>
            <Input
              id="event-prefix"
              name="prefix"
              defaultValue={filters.prefix ?? ''}
              autoComplete="off"
            />
          </div>
          <div className="flex items-end gap-2">
            <Button type="submit" variant="primary" className="flex-1">
              Apply
            </Button>
            <Button
              variant="ghost"
              onClick={() =>
                update({ bucket: null, type: null, prefix: null, after_time: null, after_id: null })
              }
            >
              Clear
            </Button>
          </div>
        </form>

        {events.isError ? (
          <ErrorState error={events.error} onRetry={() => void events.refetch()} />
        ) : events.isPending ? (
          <TableSkeleton columns={5} />
        ) : events.data.events.length === 0 ? (
          <EmptyState title="No events" description="No storage events match these filters." />
        ) : (
          <TableShell>
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Time</TableHead>
                  <TableHead>Type</TableHead>
                  <TableHead>Bucket</TableHead>
                  <TableHead>Object</TableHead>
                  <TableHead>Size</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {events.data.events.map((event) => (
                  <TableRow key={event.id}>
                    <TableCell className="whitespace-nowrap type-meta">
                      <time dateTime={event.time}>{formatDateTime(event.time)}</time>
                    </TableCell>
                    <TableCell>
                      <Badge tone="neutral" className="font-mono">
                        {event.type}
                      </Badge>
                    </TableCell>
                    <TableCell className="text-xs">{event.bucket}</TableCell>
                    <TableCell className="max-w-xs truncate text-xs" title={event.object ?? ''}>
                      {event.object ?? '—'}
                    </TableCell>
                    <TableCell className="tabular-nums text-xs">
                      {event.size === null ? '—' : formatBytes(event.size)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableShell>
        )}
      </Card>

      <div className="flex items-center justify-end gap-2">
        <Button
          size="sm"
          variant="secondary"
          disabled={filters.afterTime === null}
          onClick={() => update({ after_time: null, after_id: null })}
        >
          <ChevronLeft aria-hidden />
          First page
        </Button>
        <Button
          size="sm"
          variant="secondary"
          disabled={!events.data?.next_time}
          onClick={() =>
            update({
              after_time: events.data?.next_time ?? null,
              after_id: events.data?.next_id ?? null,
            })
          }
        >
          Next page
          <ChevronRight aria-hidden />
        </Button>
      </div>
    </>
  );
}
