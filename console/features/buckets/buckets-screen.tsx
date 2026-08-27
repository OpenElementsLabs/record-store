'use client';

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { createColumnHelper } from '@tanstack/react-table';
import { MoreHorizontal, Plus, Search } from 'lucide-react';
import Link from 'next/link';
import { useRouter, useSearchParams } from 'next/navigation';
import * as React from 'react';
import { toast } from 'sonner';

import { ConfirmDialog } from '@/components/confirm-dialog';
import { DataTable, type DataTableFeatures } from '@/components/data-table';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
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
import { CreateBucketDialog } from '@/features/buckets/create-bucket-dialog';
import { usePermissions } from '@/features/system/deployment';
import { queryKeys } from '@/hooks/use-system';
import { deleteBucket, fetchBuckets } from '@/lib/api/buckets';
import { formatBytes, formatCount, formatDate } from '@/lib/format';
import { readString } from '@/lib/search-params';
import type { Bucket } from '@/types/api';

const column = createColumnHelper<DataTableFeatures, Bucket>();

export function BucketsScreen() {
  const router = useRouter();
  const client = useQueryClient();
  const permissions = usePermissions();
  const [filter, setFilter] = React.useState('');
  const params = useSearchParams();
  const [creating, setCreating] = React.useState(() => readString(params, 'create', '') === '1');
  const [pendingDelete, setPendingDelete] = React.useState<Bucket | null>(null);

  const buckets = useQuery({
    queryKey: queryKeys.buckets,
    queryFn: ({ signal }) => fetchBuckets(signal),
  });

  const removal = useMutation({
    mutationFn: (name: string) => deleteBucket(name),
    onSuccess: async (_result, name) => {
      toast.success(`Bucket ${name} deleted`);
      setPendingDelete(null);
      await client.invalidateQueries({ queryKey: queryKeys.buckets });
    },
  });

  /**
   * Bucket counts are small, so filtering happens in the browser over the full
   * list. The label says "filter" rather than "search" because that is exactly
   * what it does.
   */
  const rows = React.useMemo(() => {
    const all = buckets.data ?? [];
    const needle = filter.trim().toLowerCase();
    if (needle.length === 0) return all;
    return all.filter((bucket) => bucket.name.toLowerCase().includes(needle));
  }, [buckets.data, filter]);

  const columns = React.useMemo(
    () =>
      column.columns([
        column.accessor('name', {
          header: 'Name',
          cell: ({ row }) => (
            <Link
              href={`/buckets/${encodeURIComponent(row.original.name)}`}
              className="font-medium text-ink hover:underline"
              onClick={(event) => event.stopPropagation()}
            >
              {row.original.name}
            </Link>
          ),
        }),
        column.accessor('object_count', {
          header: 'Objects',
          cell: ({ getValue }) => <span className="tabular-nums">{formatCount(getValue())}</span>,
        }),
        column.accessor('logical_bytes', {
          header: 'Size',
          cell: ({ getValue }) => <span className="tabular-nums">{formatBytes(getValue())}</span>,
        }),
        column.accessor('versioning', {
          header: 'Versioning',
          cell: ({ getValue }) => {
            const state = getValue();
            return (
              <StatusBadge
                level={
                  state === 'enabled' ? 'healthy' : state === 'suspended' ? 'paused' : 'disabled'
                }
                label={
                  state === 'enabled' ? 'Enabled' : state === 'suspended' ? 'Suspended' : 'Disabled'
                }
              />
            );
          },
        }),
        column.accessor('created_at', {
          header: 'Created',
          cell: ({ getValue }) => (
            <time dateTime={getValue()} title={getValue()}>
              {formatDate(getValue())}
            </time>
          ),
        }),
        column.display({
          id: 'actions',
          header: () => <span className="sr-only">Actions</span>,
          cell: ({ row }) => (
            <div className="flex justify-end" onClick={(event) => event.stopPropagation()}>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label={`Actions for ${row.original.name}`}
                  >
                    <MoreHorizontal aria-hidden />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent>
                  <DropdownMenuItem
                    onSelect={() =>
                      router.push(`/buckets/${encodeURIComponent(row.original.name)}`)
                    }
                  >
                    Browse objects
                  </DropdownMenuItem>
                  {permissions.manage_buckets ? (
                    <DropdownMenuItem destructive onSelect={() => setPendingDelete(row.original)}>
                      Delete bucket
                    </DropdownMenuItem>
                  ) : null}
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          ),
        }),
      ]),
    [permissions.manage_buckets, router],
  );

  return (
    <>
      <PageHeader
        title="Buckets"
        description="Buckets group objects and own their versioning and lifecycle settings."
        actions={
          permissions.manage_buckets ? (
            <Button variant="primary" onClick={() => setCreating(true)}>
              <Plus aria-hidden />
              Create bucket
            </Button>
          ) : null
        }
      />

      <Card>
        {buckets.isError ? (
          <ErrorState error={buckets.error} onRetry={() => void buckets.refetch()} />
        ) : (
          <>
            {(buckets.data?.length ?? 0) > 0 ? (
              <div className="flex items-center gap-2 border-b border-border px-3 py-2">
                <Search aria-hidden className="size-4 text-ink-subtle" />
                <Input
                  value={filter}
                  onChange={(event) => setFilter(event.target.value)}
                  placeholder="Filter buckets by name"
                  aria-label="Filter buckets by name"
                  className="h-8 border-0 bg-transparent px-0"
                />
              </div>
            ) : null}
            <DataTable
              data={rows}
              columns={columns}
              rowId={(bucket) => bucket.id}
              loading={buckets.isPending}
              initialSorting={[{ id: 'name', desc: false }]}
              empty={
                filter.length > 0 ? (
                  <EmptyState
                    title="No matching buckets"
                    description={`No bucket name contains “${filter}”.`}
                  />
                ) : (
                  <EmptyState
                    title="No buckets yet"
                    description="Create a bucket to start storing objects."
                    action={
                      permissions.manage_buckets ? (
                        <Button variant="primary" onClick={() => setCreating(true)}>
                          <Plus aria-hidden />
                          Create bucket
                        </Button>
                      ) : undefined
                    }
                  />
                )
              }
            />
          </>
        )}
      </Card>

      <CreateBucketDialog open={creating} onOpenChange={setCreating} />

      <ConfirmDialog
        open={pendingDelete !== null}
        onOpenChange={(open) => {
          if (!open) {
            setPendingDelete(null);
            removal.reset();
          }
        }}
        title={`Delete bucket ${pendingDelete?.name ?? ''}?`}
        description="The bucket record is removed. Record Store refuses this while the bucket still holds object versions."
        consequence={
          pendingDelete && pendingDelete.version_count > 0
            ? `This bucket still holds ${formatCount(pendingDelete.version_count)} object version(s). Delete them first.`
            : undefined
        }
        confirmLabel="Delete bucket"
        pending={removal.isPending}
        error={removal.error}
        onConfirm={() => {
          if (pendingDelete) removal.mutate(pendingDelete.name);
        }}
      />
    </>
  );
}
