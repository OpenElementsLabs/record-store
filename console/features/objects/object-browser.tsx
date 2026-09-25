'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  ChevronLeft,
  ChevronRight,
  Copy,
  Download,
  Eye,
  File as FileIcon,
  Folder,
  History,
  MoreHorizontal,
  Share2,
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
import { Input } from '@/components/ui/input';
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
import { useCapabilities, usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { ApiError } from '@/lib/api/error';
import { fetchBuckets } from '@/lib/api/buckets';
import { deleteObject, fetchObjects, objectContentUrl } from '@/lib/api/objects';
import { formatBytes, formatCount, formatDateTime, keyBasename, keySegments } from '@/lib/format';
import { mergeSearch, readInt, readString } from '@/lib/search-params';
import type { ObjectSummary, VersioningState } from '@/types/api';

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
 * directories: Record Store stores flat keys. Folders therefore appear and disappear with
 * the objects inside them, which is why they are rendered distinctly from
 * objects rather than as the same kind of row.
 */
export function ObjectBrowser({ bucket }: { readonly bucket: string }) {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const client = useQueryClient();
  const permissions = usePermissions();
  const capabilities = useCapabilities();

  const prefix = normalisePrefix(readString(params, 'prefix', ''));
  const limit = readInt(params, 'limit', 50, 25, 200);
  const cursor = readString(params, 'cursor', '') || null;
  // Finding is a different listing, not a filter over the current one: it drops
  // the delimiter so the whole bucket is scanned instead of one folder level.
  // It lives in the URL so a result set survives navigation and can be shared.
  const find = readString(params, 'find', '');
  const searching = find !== '';

  const [pendingDelete, setPendingDelete] = React.useState<ObjectSummary | null>(null);
  const [copying, setCopying] = React.useState<string | null>(null);
  // Selection is keyed by object key and cleared whenever the listing location
  // changes, which only happens through `navigate`.
  const [selected, setSelected] = React.useState<readonly string[]>([]);
  const [batch, setBatch] = React.useState<BatchProgress | null>(null);
  const dropRef = React.useRef<HTMLDivElement | null>(null);
  const [dragging, setDragging] = React.useState(false);
  // Files held back because they would land on a key this listing already
  // shows. Kept whole so confirming uploads exactly what was chosen.
  const [conflicting, setConflicting] = React.useState<readonly File[] | null>(null);

  const listing = useQuery({
    queryKey: [...queryKeys.objects(bucket, searching ? find : prefix, cursor), limit, searching],
    queryFn: ({ signal }) =>
      fetchObjects(
        {
          bucket,
          prefix: searching ? find : prefix,
          // Omitting the delimiter is what makes a find span every folder; with
          // it, the server would collapse anything deeper into prefix rows.
          ...(searching ? {} : { delimiter: '/' }),
          continuationToken: cursor,
          limit,
        },
        signal,
      ),
  });

  // The console already knows whether this bucket versions its objects, so it
  // can state what an upload will do instead of telling the operator to go and
  // find out. Only fetched when they can actually upload.
  const buckets = useQuery({
    queryKey: queryKeys.buckets,
    queryFn: ({ signal }) => fetchBuckets(signal),
    enabled: permissions.manage_objects,
    staleTime: 60_000,
  });
  // Only an array of buckets can answer the question. Anything else — a failed
  // lookup, or a payload that is not what this expects — is reported as unknown
  // rather than guessed at, because guessing "versioning is off" would tell an
  // operator their upload replaces content when it may not.
  const versioning: VersioningState | 'unknown' = Array.isArray(buckets.data)
    ? (buckets.data.find((entry) => entry.name === bucket)?.versioning ?? 'unknown')
    : 'unknown';

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
  const { setSettledHandler } = uploads;

  // Refresh the listing once the queue drains so new objects appear without the
  // operator reloading the page.
  React.useEffect(() => {
    setSettledHandler(() => {
      void client.invalidateQueries({ queryKey: ['buckets', bucket, 'objects'] });
      void client.invalidateQueries({ queryKey: queryKeys.buckets });
    });
    return () => setSettledHandler(null);
  }, [bucket, client, setSettledHandler]);

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

  /**
   * Starts an upload, pausing first when it would destroy something.
   *
   * Two conditions, and both are needed. The key must already be here — the
   * check can only see the loaded page, so it proves a collision and never
   * proves the absence of one, which is why a clean check passes silently
   * instead of announcing "no conflicts". And the overwrite must actually cost
   * something: with versioning on, replacing a key keeps the previous content
   * in history, so a confirmation would be friction in front of a safe action
   * and would train the reader to dismiss the dialog that matters.
   */
  function submit(files: readonly File[]) {
    if (files.length === 0) return;
    const existing = new Set(pageKeys);
    const collisions = files.filter((file) => existing.has(`${prefix}${file.name}`));
    if (collisions.length > 0 && overwriteDestroys(versioning)) {
      setConflicting(files);
      return;
    }
    uploads.enqueue(bucket, prefix, [...files]);
  }

  // Uploading is offered only in a folder. Find results span the bucket and
  // have no folder of their own, so a drop there would land at the bucket root
  // with its overwrite check made against the wrong listing.
  const acceptsUploads = permissions.manage_objects && !searching;

  function onDrop(event: React.DragEvent) {
    event.preventDefault();
    setDragging(false);
    if (!acceptsUploads) return;
    submit(Array.from(event.dataTransfer.files));
  }

  return (
    <div className="space-y-4">
      {permissions.manage_objects && !searching ? (
        <p id="upload-behavior" className="type-meta">
          Files upload immediately using their names under this folder.{' '}
          <UploadConsequence versioning={buckets.isPending ? 'loading' : versioning} /> Interrupted
          uploads do not resume.
        </p>
      ) : null}
      <FindBar
        value={find}
        bucket={bucket}
        onSubmit={(next) =>
          // A find replaces the folder location rather than narrowing it: the
          // results span the bucket, so keeping a prefix would be a lie.
          navigate({ find: next || null, cursor: null, prefix: next ? null : prefix || null })
        }
      />
      <div className="flex flex-wrap items-center justify-between gap-3">
        {searching ? <FindScope bucket={bucket} find={find} /> : <Breadcrumbs items={crumbs} />}
        {permissions.manage_objects && !searching ? (
          <label className="inline-flex rounded-control focus-within:ring-2 focus-within:ring-accent">
            <input
              type="file"
              multiple
              aria-label="Upload files"
              aria-describedby="upload-behavior"
              className="sr-only"
              onChange={(event) => {
                submit(Array.from(event.target.files ?? []));
                event.target.value = '';
              }}
            />
            <span className="inline-flex h-9 cursor-pointer items-center gap-2 rounded-control bg-accent px-4 text-sm font-medium text-accent-ink hover:bg-accent-hover">
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
          if (!acceptsUploads) return;
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
          searching ? (
            <EmptyState
              title={`No keys in ${bucket} begin with “${find}”`}
              description={
                'Matching is on the beginning of the whole key, including its folders — so ' +
                '“report.pdf” does not match “documents/report.pdf”. Try a shorter beginning, ' +
                'or include the folder.'
              }
            />
          ) : (
            <EmptyState
              title={prefix ? 'Nothing under this prefix' : 'This bucket is empty'}
              description={
                permissions.manage_objects
                  ? 'Upload a file, or drop files onto this panel, to store your first object.'
                  : 'No objects are stored here yet.'
              }
            />
          )
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
                    <TableCell className={searching ? 'whitespace-nowrap' : undefined}>
                      <Link
                        href={objectHref(bucket, object.key)}
                        className="inline-flex items-center gap-2 type-body hover:underline"
                      >
                        <FileIcon aria-hidden className="size-4 shrink-0 text-ink-subtle" />
                        {searching ? (
                          // Results span folders, so the basename alone would be
                          // ambiguous between two keys with the same file name.
                          // Kept on one line: the table shell scrolls, whereas
                          // wrapping a long key inside a narrow column breaks it
                          // one character per line and makes it unreadable.
                          <span className="shrink-0 whitespace-nowrap">{object.key}</span>
                        ) : (
                          keyBasename(object.key)
                        )}
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
                            {/*
                              Clicking the name opens the object; these are the
                              secondary actions, so the row stays a row rather
                              than a toolbar.
                            */}
                            <DropdownMenuItem asChild>
                              <Link href={objectHref(bucket, object.key)}>
                                <Eye aria-hidden /> Open
                              </Link>
                            </DropdownMenuItem>
                            <DropdownMenuItem asChild>
                              {/*
                                The browser fetches bytes straight from Record Store, so a
                                large download never passes through this app.
                              */}
                              <a href={objectContentUrl(bucket, object.key)} download>
                                <Download aria-hidden /> Download
                              </a>
                            </DropdownMenuItem>
                            {permissions.manage_sharing ? (
                              <DropdownMenuItem asChild>
                                <Link href={`${objectHref(bucket, object.key)}?tab=sharing`}>
                                  <Share2 aria-hidden /> Share or embed
                                </Link>
                              </DropdownMenuItem>
                            ) : null}
                            {capabilities.versioning ? (
                              <DropdownMenuItem asChild>
                                <Link href={`${objectHref(bucket, object.key)}?tab=versions`}>
                                  <History aria-hidden /> Versions
                                </Link>
                              </DropdownMenuItem>
                            ) : null}
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

      <ConfirmDialog
        open={conflicting !== null}
        onOpenChange={(open) => {
          // Dismissing uploads nothing: the files stay unsent rather than being
          // queued behind a dialog the operator closed.
          if (!open) setConflicting(null);
        }}
        title={
          conflicting === null
            ? ''
            : overwriteTitle(
                conflicting.filter((file) => pageKeys.includes(`${prefix}${file.name}`)),
              )
        }
        description={
          conflicting === null
            ? ''
            : conflicting
                .filter((file) => pageKeys.includes(`${prefix}${file.name}`))
                .map((file) => file.name)
                .join(', ')
        }
        consequence={overwriteConsequence(versioning)}
        confirmLabel="Upload anyway"
        onConfirm={() => {
          if (conflicting) uploads.enqueue(bucket, prefix, [...conflicting]);
          setConflicting(null);
        }}
      />
    </div>
  );
}

/**
 * Whether replacing an existing key here loses the previous content.
 *
 * Unknown counts as destructive: the console cannot rule out loss, and the
 * safer error is to ask.
 */
function overwriteDestroys(versioning: VersioningState | 'unknown'): boolean {
  return versioning !== 'enabled';
}

/** Names how many of the chosen files already exist here. */
function overwriteTitle(collisions: readonly File[]): string {
  return collisions.length === 1
    ? `${collisions[0]?.name} already exists here`
    : `${collisions.length} of these files already exist here`;
}

/**
 * States what uploading over an existing key costs in this bucket.
 *
 * Written as a consequence rather than a warning because the three versioning
 * states differ in whether anything is actually lost, and a single "this will
 * overwrite" would be wrong in the enabled case and too mild in the others.
 */
function overwriteConsequence(versioning: VersioningState | 'unknown'): string {
  if (versioning === 'enabled') {
    return 'Versioning is on, so each of these adds a new current version and the existing content stays in history.';
  }
  if (versioning === 'suspended') {
    return 'Versioning is suspended, so each of these replaces the null version permanently. Versions created before suspension are kept.';
  }
  if (versioning === 'disabled') {
    return 'Versioning is off, so each of these permanently replaces the stored content. The previous bytes cannot be recovered.';
  }
  return 'This bucket’s versioning could not be read, so whether the existing content survives is unknown.';
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
          className="h-8 rounded-control border border-border-strong bg-surface px-2 text-xs text-ink"
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

/**
 * The detail route for one key.
 *
 * Each segment is encoded on its own so a key containing slashes, spaces, or
 * anything else that looks like path structure survives the round trip.
 */
function objectHref(bucket: string, key: string): string {
  return `/buckets/${encodeURIComponent(bucket)}/objects/${key
    .split('/')
    .map(encodeURIComponent)
    .join('/')}`;
}

/** Normalises a prefix so it is either empty or ends with a delimiter. */
/**
 * Finds objects by the beginning of their key, across the whole bucket.
 *
 * This is the only matching the storage layer can do: keys are held in one
 * ordered index and a listing is a range scan over it. There is no word index,
 * no metadata index, and no substring matching, so the control says "begins
 * with" rather than "search" — a box labelled Search that quietly failed to
 * find `report.pdf` inside `documents/` would be worse than no box at all.
 */
function FindBar({
  value,
  bucket,
  onSubmit,
}: {
  readonly value: string;
  readonly bucket: string;
  readonly onSubmit: (next: string) => void;
}) {
  const [draft, setDraft] = React.useState(value);
  // The URL is the source of truth; when it moves the box follows it.
  const [synced, setSynced] = React.useState(value);
  if (synced !== value) {
    setSynced(value);
    setDraft(value);
  }

  return (
    <form
      className="flex flex-wrap items-end gap-2"
      role="search"
      onSubmit={(event) => {
        event.preventDefault();
        onSubmit(draft.trim());
      }}
    >
      <div className="w-full max-w-sm space-y-1.5">
        <label htmlFor="object-find" className="type-label">
          Find keys beginning with
        </label>
        <Input
          id="object-find"
          type="search"
          value={draft}
          placeholder="documents/2026"
          aria-describedby="object-find-scope"
          onChange={(event) => setDraft(event.target.value)}
        />
      </div>
      <Button type="submit" variant="secondary">
        Find
      </Button>
      {value ? (
        <Button type="button" variant="ghost" onClick={() => onSubmit('')}>
          Clear
        </Button>
      ) : null}
      <p id="object-find-scope" className="w-full type-meta-subtle">
        Searches every folder in {bucket} by the start of the whole key. It does not match words
        inside a name, metadata, or file contents.
      </p>
    </form>
  );
}

/** Says what the rows below are, now that they are not a folder. */
function FindScope({ bucket, find }: { readonly bucket: string; readonly find: string }) {
  return (
    <p className="type-body" role="status">
      Keys in <span className="font-medium">{bucket}</span> beginning with{' '}
      <span className="font-mono">{find}</span>
    </p>
  );
}

/**
 * States what uploading here will actually do to an existing key.
 *
 * The three versioning states have genuinely different consequences, and the
 * difference is the whole question an operator is asking before they drop a
 * file onto a key that already exists. Saying "check bucket versioning" put
 * that lookup back on them for information the console already had.
 *
 * `undefined` means the bucket record has not loaded yet, which is not the same
 * as versioning being off — so it says nothing rather than guessing wrong in the
 * more dangerous direction.
 */
function UploadConsequence({
  versioning,
}: {
  readonly versioning: VersioningState | 'unknown' | 'loading';
}) {
  if (versioning === 'enabled') {
    return (
      <>Versioning is on: uploading to an existing key adds a version and keeps the previous one.</>
    );
  }
  if (versioning === 'suspended') {
    return (
      <>
        Versioning is suspended: uploading to an existing key replaces its null version permanently,
        while versions created earlier are kept.
      </>
    );
  }
  if (versioning === 'disabled') {
    return <>Versioning is off: uploading to an existing key replaces its content permanently.</>;
  }
  if (versioning === 'loading') return <>Checking this bucket’s versioning…</>;
  return (
    <>
      This bucket’s versioning could not be read, so whether an upload to an existing key keeps the
      previous content is unknown.
    </>
  );
}

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
