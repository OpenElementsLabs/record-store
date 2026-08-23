'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Download, MoreHorizontal, RotateCcw } from 'lucide-react';
import { usePathname, useRouter, useSearchParams } from 'next/navigation';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { StatusBadge } from '@/components/status-badge';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
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
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import {
  deleteObjectVersion,
  fetchObjectVersions,
  objectContentUrl,
  restoreObjectVersion,
} from '@/lib/api/objects';
import { formatBytes, formatDateTime, shortenIdentifier } from '@/lib/format';
import { mergeSearch, readString } from '@/lib/search-params';
import type { ObjectVersionEntry } from '@/types/api';

/**
 * Version history for a bucket.
 *
 * Delete markers are shown as first-class entries rather than hidden, because
 * their presence is what explains why a key appears absent.
 */
export function ObjectVersions({
  bucket,
  prefixOverride,
}: {
  readonly bucket: string;
  /**
   * Restricts the list to one key.
   *
   * Object detail embeds this list for a single object, where the prefix comes
   * from the route rather than from a filter the operator typed.
   */
  readonly prefixOverride?: string;
}) {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const client = useQueryClient();
  const permissions = usePermissions();

  // A caller-supplied scope wins over the URL filter: object detail is already
  // looking at one key, so a stale filter must not widen the list.
  const prefix = prefixOverride ?? readString(params, 'vprefix', '');
  const [draft, setDraft] = React.useState(prefix);
  const [pendingDelete, setPendingDelete] = React.useState<ObjectVersionEntry | null>(null);

  const versions = useQuery({
    queryKey: queryKeys.objectVersions(bucket, prefix),
    queryFn: ({ signal }) => fetchObjectVersions({ bucket, prefix, limit: 100 }, signal),
  });

  const removal = useMutation({
    mutationFn: (entry: ObjectVersionEntry) =>
      deleteObjectVersion(bucket, entry.key, entry.version_id),
    onSuccess: async () => {
      toast.success('Version permanently deleted');
      setPendingDelete(null);
      await client.invalidateQueries({ queryKey: ['buckets', bucket] });
    },
  });

  const restore = useMutation({
    mutationFn: (entry: ObjectVersionEntry) =>
      restoreObjectVersion(bucket, entry.key, entry.version_id),
    onSuccess: async () => {
      toast.success('Version restored as current');
      await client.invalidateQueries({ queryKey: ['buckets', bucket] });
    },
  });

  return (
    <div className="space-y-4">
      <form
        className="flex items-end gap-2"
        onSubmit={(event) => {
          event.preventDefault();
          router.push(`${pathname}${mergeSearch(params, { vprefix: draft || null })}`);
        }}
      >
        <div className="w-full max-w-sm space-y-1.5">
          <label htmlFor="version-prefix" className="text-xs font-medium text-ink">
            Key prefix
          </label>
          <Input
            id="version-prefix"
            value={draft}
            placeholder="documents/"
            onChange={(event) => setDraft(event.target.value)}
          />
        </div>
        <Button type="submit" size="md">
          Filter
        </Button>
      </form>

      <Card>
        {versions.isError ? (
          <ErrorState error={versions.error} onRetry={() => void versions.refetch()} />
        ) : versions.isPending ? (
          <TableSkeleton columns={5} />
        ) : versions.data.versions.length === 0 ? (
          <EmptyState
            title="No versions"
            description="No object versions match this prefix in this bucket."
          />
        ) : (
          <TableShell>
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead>Key</TableHead>
                  <TableHead>Version</TableHead>
                  <TableHead>State</TableHead>
                  <TableHead>Size</TableHead>
                  <TableHead>Created</TableHead>
                  <TableHead>
                    <span className="sr-only">Actions</span>
                  </TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {versions.data.versions.map((entry) => (
                  <TableRow key={`${entry.key}:${entry.version_id}`}>
                    <TableCell className="max-w-xs truncate" title={entry.key}>
                      {entry.key}
                    </TableCell>
                    <TableCell
                      className="font-mono text-xs text-ink-muted"
                      title={entry.version_id}
                    >
                      {shortenIdentifier(entry.version_id, 6)}
                    </TableCell>
                    <TableCell>
                      <div className="flex flex-wrap items-center gap-1">
                        {entry.is_latest ? <StatusBadge level="healthy" label="Current" /> : null}
                        {entry.is_delete_marker ? (
                          <StatusBadge level="disabled" label="Delete marker" />
                        ) : null}
                        {entry.is_null ? <StatusBadge level="unknown" label="Null" /> : null}
                      </div>
                    </TableCell>
                    <TableCell className="tabular-nums">
                      {entry.size === null ? '—' : formatBytes(entry.size)}
                    </TableCell>
                    <TableCell className="text-xs text-ink-muted">
                      <time dateTime={entry.created_at} title={entry.created_at}>
                        {formatDateTime(entry.created_at)}
                      </time>
                    </TableCell>
                    <TableCell>
                      <div className="flex justify-end">
                        <DropdownMenu>
                          <DropdownMenuTrigger asChild>
                            <Button
                              variant="ghost"
                              size="icon"
                              aria-label={`Actions for version ${entry.version_id}`}
                            >
                              <MoreHorizontal aria-hidden />
                            </Button>
                          </DropdownMenuTrigger>
                          <DropdownMenuContent>
                            {entry.is_delete_marker ? null : (
                              <DropdownMenuItem asChild>
                                <a href={objectContentUrl(bucket, entry.key)} download>
                                  <Download aria-hidden /> Download current
                                </a>
                              </DropdownMenuItem>
                            )}
                            {permissions.manage_objects &&
                            !entry.is_latest &&
                            !entry.is_delete_marker ? (
                              <DropdownMenuItem
                                onSelect={() => restore.mutate(entry)}
                                disabled={restore.isPending}
                              >
                                <RotateCcw aria-hidden /> Restore as current
                              </DropdownMenuItem>
                            ) : null}
                            {permissions.manage_objects ? (
                              <DropdownMenuItem
                                destructive
                                onSelect={() => setPendingDelete(entry)}
                              >
                                Delete permanently
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

      {versions.data?.next_key_marker ? (
        <p className="text-xs text-ink-subtle">
          More versions match this prefix than are shown. Narrow the prefix to see them.
        </p>
      ) : null}

      {restore.error ? <ErrorState error={restore.error} /> : null}

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            removal.reset();
          }
        }}
        strength="type-to-confirm"
        expectedText={pendingDelete?.version_id.slice(0, 8)}
        title="Permanently delete this version?"
        description={`Version ${pendingDelete?.version_id ?? ''} of ${pendingDelete?.key ?? ''} will be removed.`}
        consequence="Permanent version deletion cannot be undone and does not create a delete marker."
        confirmLabel="Delete permanently"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => {
          if (pendingDelete) removal.mutate(pendingDelete);
        }}
      />
    </div>
  );
}
