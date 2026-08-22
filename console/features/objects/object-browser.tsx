'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  ChevronLeft,
  ChevronRight,
  Download,
  File as FileIcon,
  Folder,
  MoreHorizontal,
  Upload,
} from 'lucide-react';
import Link from 'next/link';
import { usePathname, useRouter, useSearchParams } from 'next/navigation';
import * as React from 'react';
import { toast } from 'sonner';

import { Breadcrumbs, type Crumb } from '@/components/breadcrumbs';
import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
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
import { UploadPanel } from '@/features/objects/upload-panel';
import { useUploadManager } from '@/features/objects/upload-manager';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { deleteObject, fetchObjects, objectContentUrl } from '@/lib/api/objects';
import { formatBytes, formatDateTime, keyBasename, keySegments } from '@/lib/format';
import { mergeSearch, readInt, readString } from '@/lib/search-params';
import type { ObjectSummary } from '@/types/api';

const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;

/**
 * Browses a bucket by logical prefix.
 *
 * Prefixes are groupings produced by applying `/` as a delimiter, not
 * directories: OES stores flat keys. Folders therefore appear and disappear with
 * the objects inside them, which is why they are rendered distinctly from
 * objects rather than as the same kind of row.
 */
export function ObjectBrowser({ bucket }: { readonly bucket: string }) {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const client = useQueryClient();
  const permissions = usePermissions();

  const prefix = normalisePrefix(readString(params, 'prefix', ''));
  const limit = readInt(params, 'limit', 50, 25, 200);
  const cursor = readString(params, 'cursor', '') || null;

  const [pendingDelete, setPendingDelete] = React.useState<ObjectSummary | null>(null);
  const dropRef = React.useRef<HTMLDivElement | null>(null);
  const [dragging, setDragging] = React.useState(false);

  const listing = useQuery({
    queryKey: queryKeys.objects(bucket, prefix, cursor),
    queryFn: ({ signal }) =>
      fetchObjects({ bucket, prefix, delimiter: '/', continuationToken: cursor, limit }, signal),
  });

  const uploads = useUploadManager();

  // Refresh the listing once the queue drains so new objects appear without the
  // operator reloading the page.
  React.useEffect(() => {
    uploads.setSettledHandler(() => {
      void client.invalidateQueries({ queryKey: ['buckets', bucket, 'objects'] });
      void client.invalidateQueries({ queryKey: queryKeys.buckets });
    });
    return () => uploads.setSettledHandler(null);
  }, [bucket, client, uploads]);

  const removal = useMutation({
    mutationFn: (key: string) => deleteObject(bucket, key),
    onSuccess: async (_result, key) => {
      toast.success(`Deleted ${keyBasename(key)}`);
      setPendingDelete(null);
      await client.invalidateQueries({ queryKey: ['buckets', bucket, 'objects'] });
      await client.invalidateQueries({ queryKey: queryKeys.buckets });
    },
  });

  function navigate(updates: Record<string, string | number | null>) {
    router.push(`${pathname}${mergeSearch(params, updates)}`);
  }

  function openPrefix(next: string) {
    // Changing location invalidates the cursor, which belongs to the old page.
    navigate({ prefix: next || null, cursor: null });
  }

  const crumbs: Crumb[] = [
    { label: bucket, href: `/buckets/${encodeURIComponent(bucket)}` },
    ...keySegments(prefix).map((segment, index, all) => ({
      label: segment,
      href: `/buckets/${encodeURIComponent(bucket)}${mergeSearch(new URLSearchParams(), {
        prefix: `${all.slice(0, index + 1).join('/')}/`,
      })}`,
    })),
  ];

  function onDrop(event: React.DragEvent) {
    event.preventDefault();
    setDragging(false);
    if (!permissions.manage_objects) return;
    const files = Array.from(event.dataTransfer.files);
    if (files.length > 0) uploads.enqueue(bucket, prefix, files);
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <Breadcrumbs items={crumbs} />
        {permissions.manage_objects ? (
          <label className="inline-flex">
            <input
              type="file"
              multiple
              className="sr-only"
              onChange={(event) => {
                const files = Array.from(event.target.files ?? []);
                if (files.length > 0) uploads.enqueue(bucket, prefix, files);
                event.target.value = '';
              }}
            />
            <span className="inline-flex h-9 cursor-pointer items-center gap-2 rounded-[--radius-control] bg-accent px-4 text-sm font-medium text-accent-ink hover:bg-accent-hover">
              <Upload aria-hidden className="size-4" />
              Upload files
            </span>
          </label>
        ) : null}
      </div>

      <UploadPanel
        tasks={uploads.tasks}
        onCancel={uploads.cancel}
        onRetry={uploads.retry}
        onClear={uploads.clearFinished}
      />

      <Card
        ref={dropRef}
        onDragOver={(event) => {
          if (!permissions.manage_objects) return;
          event.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={onDrop}
        className={dragging ? 'ring-2 ring-accent' : undefined}
      >
        {listing.isError ? (
          <ErrorState error={listing.error} onRetry={() => void listing.refetch()} />
        ) : listing.isPending ? (
          <TableSkeleton columns={4} />
        ) : listing.data.prefixes.length === 0 && listing.data.objects.length === 0 ? (
          <EmptyState
            title={prefix ? 'Nothing under this prefix' : 'This bucket is empty'}
            description={
              permissions.manage_objects
                ? 'Upload a file, or drop files onto this panel, to store your first object.'
                : 'No objects are stored here yet.'
            }
          />
        ) : (
          <TableShell>
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Name</TableHead>
                  <TableHead>Size</TableHead>
                  <TableHead>Type</TableHead>
                  <TableHead>Modified</TableHead>
                  <TableHead>
                    <span className="sr-only">Actions</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {listing.data.prefixes.map((entry) => (
                  <TableRow key={`prefix:${entry}`}>
                    <TableCell colSpan={4}>
                      <button
                        type="button"
                        onClick={() => openPrefix(entry)}
                        className="inline-flex items-center gap-2 text-sm font-medium text-ink hover:underline"
                      >
                        <Folder aria-hidden className="size-4 text-ink-subtle" />
                        {trailingSegment(entry)}
                      </button>
                    </TableCell>
                    <TableCell />
                  </TableRow>
                ))}
                {listing.data.objects.map((object) => (
                  <TableRow key={object.key}>
                    <TableCell>
                      <Link
                        href={`/buckets/${encodeURIComponent(bucket)}/objects/${object.key
                          .split('/')
                          .map(encodeURIComponent)
                          .join('/')}`}
                        className="inline-flex items-center gap-2 text-sm text-ink hover:underline"
                      >
                        <FileIcon aria-hidden className="size-4 text-ink-subtle" />
                        {keyBasename(object.key)}
                      </Link>
                    </TableCell>
                    <TableCell className="tabular-nums">{formatBytes(object.size)}</TableCell>
                    <TableCell className="text-xs text-ink-muted">
                      {object.content_type ?? '—'}
                    </TableCell>
                    <TableCell className="text-xs text-ink-muted">
                      <time dateTime={object.modified_at} title={object.modified_at}>
                        {formatDateTime(object.modified_at)}
                      </time>
                    </TableCell>
                    <TableCell>
                      <div className="flex justify-end">
                        <DropdownMenu>
                          <DropdownMenuTrigger asChild>
                            <Button
                              variant="ghost"
                              size="icon"
                              aria-label={`Actions for ${keyBasename(object.key)}`}
                            >
                              <MoreHorizontal aria-hidden />
                            </Button>
                          </DropdownMenuTrigger>
                          <DropdownMenuContent>
                            <DropdownMenuItem asChild>
                              {/*
                                The browser fetches bytes straight from OES, so a
                                large download never passes through this app.
                              */}
                              <a href={objectContentUrl(bucket, object.key)} download>
                                <Download aria-hidden /> Download
                              </a>
                            </DropdownMenuItem>
                            {permissions.manage_objects ? (
                              <DropdownMenuItem
                                destructive
                                onSelect={() => setPendingDelete(object)}
                              >
                                Delete object
                              </DropdownMenuItem>
                            ) : null}
                          </DropdownMenuContent>
                        </DropdownMenu>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </TableShell>
        )}
      </Card>

      <Pagination
        limit={limit}
        hasCursor={cursor !== null}
        nextCursor={listing.data?.next_continuation_token ?? null}
        onLimit={(next) => navigate({ limit: next, cursor: null })}
        onNext={(next) => navigate({ cursor: next })}
        onFirst={() => navigate({ cursor: null })}
      />

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            removal.reset();
          }
        }}
        title={`Delete ${pendingDelete ? keyBasename(pendingDelete.key) : ''}?`}
        description="The current version of this object is deleted."
        consequence="In a versioning-enabled bucket this adds a delete marker; otherwise the object is removed permanently."
        confirmLabel="Delete object"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => {
          if (pendingDelete) removal.mutate(pendingDelete.key);
        }}
      />
    </div>
  );
}

/**
 * Cursor pagination controls.
 *
 * The API hands out opaque forward cursors, so the console offers "next" and a
 * return to the first page rather than pretending to know page numbers.
 */
function Pagination({
  limit,
  hasCursor,
  nextCursor,
  onLimit,
  onNext,
  onFirst,
}: {
  readonly limit: number;
  readonly hasCursor: boolean;
  readonly nextCursor: string | null;
  readonly onLimit: (limit: number) => void;
  readonly onNext: (cursor: string) => void;
  readonly onFirst: () => void;
}) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <label className="flex items-center gap-2 text-xs text-ink-muted">
        Rows per page
        <select
          value={limit}
          onChange={(event) => onLimit(Number(event.target.value))}
          className="h-8 rounded-[--radius-control] border border-border-strong bg-surface px-2 text-xs text-ink"
        >
          {PAGE_SIZE_OPTIONS.map((option) => (
            <option key={option} value={option}>
              {option}
            </option>
          ))}
        </select>
      </label>
      <div className="flex items-center gap-2">
        <Button size="sm" variant="secondary" disabled={!hasCursor} onClick={onFirst}>
          <ChevronLeft aria-hidden />
          First page
        </Button>
        <Button
          size="sm"
          variant="secondary"
          disabled={nextCursor === null}
          onClick={() => nextCursor && onNext(nextCursor)}
        >
          Next page
          <ChevronRight aria-hidden />
        </Button>
      </div>
    </div>
  );
}

/** Normalises a prefix so it is either empty or ends with a delimiter. */
function normalisePrefix(value: string): string {
  if (value.length === 0) return '';
  const trimmed = value.replace(/^\/+/, '');
  return trimmed.endsWith('/') ? trimmed : `${trimmed}/`;
}

function trailingSegment(prefix: string): string {
  const segments = keySegments(prefix);
  return segments.length > 0 ? (segments[segments.length - 1] as string) : prefix;
}
