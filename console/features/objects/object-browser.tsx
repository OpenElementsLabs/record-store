'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  ChevronLeft,
  ChevronRight,
  Copy,
  Download,
  File as FileIcon,
  Folder,
  MoreHorizontal,
  Trash2,
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
import { Checkbox } from '@/components/ui/checkbox';
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
import { CopyObjectDialog } from '@/features/objects/copy-object-dialog';
import { UploadPanel } from '@/features/objects/upload-panel';
import { useUploadManager } from '@/features/objects/upload-manager';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { ApiError } from '@/lib/api/error';
import { deleteObject, fetchObjects, objectContentUrl } from '@/lib/api/objects';
import { formatBytes, formatCount, formatDateTime, keyBasename, keySegments } from '@/lib/format';
import { mergeSearch, readInt, readString } from '@/lib/search-params';
import type { ObjectSummary } from '@/types/api';

const PAGE_SIZE_OPTIONS = [25, 50, 100, 200] as const;

/**
 * How far a batch delete has got.
 *
 * The management API deletes one key per request, so a batch is a sequence of
 * independent deletions. It can therefore partly succeed, and the UI reports
 * exactly which keys failed rather than implying the whole batch was atomic.
 */
type BatchProgress = {
  readonly total: number;
  readonly completed: number;
  readonly failed: readonly { readonly key: string; readonly reason: string }[];
  readonly running: boolean;
};

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
  const [copying, setCopying] = React.useState<string | null>(null);
  // Selection is keyed by object key and cleared whenever the listing location
  // changes, which only happens through `navigate`.
  const [selected, setSelected] = React.useState<readonly string[]>([]);
  const [batch, setBatch] = React.useState<BatchProgress | null>(null);
  const dropRef = React.useRef<HTMLDivElement | null>(null);
  const [dragging, setDragging] = React.useState(false);

  const listing = useQuery({
    queryKey: queryKeys.objects(bucket, prefix, cursor),
    queryFn: ({ signal }) =>
      fetchObjects({ bucket, prefix, delimiter: '/', continuationToken: cursor, limit }, signal),
  });

  const pageKeys = React.useMemo(
    () => (listing.data?.objects ?? []).map((object) => object.key),
    [listing.data],
  );
  // Keys that vanished from the listing (deleted elsewhere, or a refetch) must
  // not stay selected and be acted on later.
  const selectedOnPage = React.useMemo(
    () => selected.filter((key) => pageKeys.includes(key)),
    [selected, pageKeys],
  );
  const selectable = permissions.manage_objects && pageKeys.length > 0;
  const allSelected = pageKeys.length > 0 && selectedOnPage.length === pageKeys.length;

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

  /**
   * Deletes the selected keys one at a time.
   *
   * Sequential rather than parallel: the API takes one key per call, and firing
   * hundreds of concurrent deletes would be a self-inflicted load spike. Each
   * failure is recorded and the run continues, so one bad key does not strand
   * the rest.
   */
  async function runBatchDelete(keys: readonly string[]) {
    setBatch({ total: keys.length, completed: 0, failed: [], running: true });
    const failed: { key: string; reason: string }[] = [];
    let completed = 0;
    for (const key of keys) {
      try {
        await deleteObject(bucket, key);
      } catch (error) {
        failed.push({
          key,
          reason: error instanceof ApiError ? error.message : 'The request failed.',
        });
      }
      completed += 1;
      setBatch({ total: keys.length, completed, failed: [...failed], running: true });
    }
    setBatch({ total: keys.length, completed, failed, running: false });
    setSelected([]);
    if (failed.length === 0) {
      toast.success(`Deleted ${formatCount(keys.length)} objects`);
    } else {
      toast.error(
        `${formatCount(failed.length)} of ${formatCount(keys.length)} objects could not be deleted`,
      );
    }
    await client.invalidateQueries({ queryKey: ['buckets', bucket, 'objects'] });
    await client.invalidateQueries({ queryKey: queryKeys.buckets });
  }

  function navigate(updates: Record<string, string | number | null>) {
    // A selection belongs to the page it was made on.
    setSelected([]);
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
                  {selectable ? (
                    <TableHead className="w-8">
                      <Checkbox
                        aria-label="Select all objects on this page"
                        checked={
                          allSelected ? true : selectedOnPage.length > 0 ? 'indeterminate' : false
                        }
                        onCheckedChange={(next) => setSelected(next === true ? pageKeys : [])}
                      />
                    </TableHead>
                  ) : null}
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
                    {selectable ? <TableCell /> : null}
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
                    {selectable ? (
                      <TableCell>
                        <Checkbox
                          aria-label={`Select ${keyBasename(object.key)}`}
                          checked={selectedOnPage.includes(object.key)}
                          onCheckedChange={(next) =>
                            setSelected((current) =>
                              next === true
                                ? [...current, object.key]
                                : current.filter((key) => key !== object.key),
                            )
                          }
                        />
                      </TableCell>
                    ) : null}
                    <TableCell>
                      <Link
                        href={`/buckets/${encodeURIComponent(bucket)}/objects/${object.key
                          .split('/')
                          .map(encodeURIComponent)
                          .join('/')}`}
                        className="inline-flex items-center gap-2 type-body hover:underline"
                      >
                        <FileIcon aria-hidden className="size-4 text-ink-subtle" />
                        {keyBasename(object.key)}
                      </Link>
                    </TableCell>
                    <TableCell className="tabular-nums">{formatBytes(object.size)}</TableCell>
                    <TableCell className="type-meta">{object.content_type ?? '—'}</TableCell>
                    <TableCell className="type-meta">
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
                              <DropdownMenuItem onSelect={() => setCopying(object.key)}>
                                <Copy aria-hidden /> Copy to…
                              </DropdownMenuItem>
                            ) : null}
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

      {selectedOnPage.length > 0 || (batch !== null && batch.failed.length > 0) ? (
        <SelectionBar
          count={selectedOnPage.length}
          batch={batch}
          onClear={() => {
            setSelected([]);
            setBatch(null);
          }}
          onDelete={() => void runBatchDelete(selectedOnPage)}
        />
      ) : null}

      <Pagination
        limit={limit}
        hasCursor={cursor !== null}
        nextCursor={listing.data?.next_continuation_token ?? null}
        onLimit={(next) => navigate({ limit: next, cursor: null })}
        onNext={(next) => navigate({ cursor: next })}
        onFirst={() => navigate({ cursor: null })}
      />

      <CopyObjectDialog
        bucket={bucket}
        objectKey={copying}
        open={copying !== null}
        onOpenChange={(next) => setCopying(next ? copying : null)}
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
      <label className="flex items-center gap-2 type-meta">
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

/**
 * Actions for the current selection.
 *
 * It reports progress against a real total and names the keys that failed,
 * because a partly-completed batch is a normal outcome when each deletion is
 * its own request.
 */
function SelectionBar({
  count,
  batch,
  onClear,
  onDelete,
}: {
  readonly count: number;
  readonly batch: BatchProgress | null;
  readonly onClear: () => void;
  readonly onDelete: () => void;
}) {
  const running = batch?.running ?? false;
  return (
    <Card>
      <div className="flex flex-wrap items-center gap-3 px-4 py-3">
        <p className="type-body" role="status">
          {running && batch
            ? `Deleting ${formatCount(batch.completed)} of ${formatCount(batch.total)}…`
            : count > 0
              ? `${formatCount(count)} selected`
              : batch
                ? `Deleted ${formatCount(batch.completed - batch.failed.length)} of ${formatCount(batch.total)}`
                : ''}
        </p>
        <div className="ml-auto flex items-center gap-2">
          <Button size="sm" variant="ghost" onClick={onClear} disabled={running}>
            {count > 0 ? 'Clear' : 'Dismiss'}
          </Button>
          {count > 0 ? (
            <Button size="sm" variant="danger" onClick={onDelete} disabled={running}>
              <Trash2 aria-hidden />
              Delete selected
            </Button>
          ) : null}
        </div>
      </div>
      {batch && !batch.running && batch.failed.length > 0 ? (
        <div className="border-t border-border px-4 py-3">
          <p className="text-xs font-medium text-danger">
            {formatCount(batch.failed.length)} could not be deleted
          </p>
          <ul className="mt-1 space-y-0.5">
            {batch.failed.map((failure) => (
              <li key={failure.key} className="type-meta">
                <span className="font-mono">{keyBasename(failure.key)}</span> — {failure.reason}
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </Card>
  );
}
