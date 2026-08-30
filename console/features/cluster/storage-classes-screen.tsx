'use client';

import { useQuery } from '@tanstack/react-query';
import { createColumnHelper } from '@tanstack/react-table';
import * as React from 'react';

import { DataTable, type DataTableFeatures } from '@/components/data-table';
import { EmptyState } from '@/components/empty-state';
import { ErrorState } from '@/components/error-state';
import { PageHeader } from '@/components/page-header';
import { Badge } from '@/components/ui/badge';
import { Card } from '@/components/ui/card';
import { queryKeys } from '@/hooks/use-system';
import { fetchStorageClasses } from '@/lib/api/cluster';
import type { DurabilityStrategy, StoragePolicy } from '@/types/cluster';

/** Describes how a class keeps data, in the terms an operator chose it in. */
function durabilityLabel(durability: DurabilityStrategy): string {
  if (durability.strategy === 'replication') {
    return `${durability.replicas} copies`;
  }
  return `${durability.profile.data_shards}+${durability.profile.parity_shards} erasure coding`;
}

const column = createColumnHelper<DataTableFeatures, StoragePolicy>();

export function StorageClassesScreen() {
  const classes = useQuery({
    queryKey: queryKeys.storageClasses,
    queryFn: ({ signal }) => fetchStorageClasses(signal),
    refetchInterval: 30_000,
  });

  const columns = React.useMemo(
    () =>
      column.columns([
        column.accessor((row) => row.class, {
          id: 'class',
          header: 'Class',
          cell: ({ row }) => (
            <div className="space-y-0.5">
              <p className="font-mono text-xs text-ink">{row.original.class}</p>
              {row.original.description ? (
                <p className="max-w-64 type-meta-subtle">{row.original.description}</p>
              ) : null}
            </div>
          ),
        }),
        column.accessor((row) => durabilityLabel(row.durability), {
          id: 'durability',
          header: 'Durability',
          cell: ({ getValue }) => <span className="text-xs text-ink">{getValue()}</span>,
        }),
        column.accessor((row) => row.failure_domain, {
          id: 'separation',
          header: 'Separation',
          cell: ({ row }) => (
            <div className="flex flex-wrap gap-1">
              <Badge tone="neutral">{row.original.failure_domain}</Badge>
              {/* Strict is worth showing: it decides whether a write fails or
                  quietly lands without the separation the class asked for. */}
              {row.original.strict_failure_domains ? <Badge tone="accent">strict</Badge> : null}
            </div>
          ),
        }),
        column.accessor((row) => row.device_filter.allowed_kinds.length, {
          id: 'devices',
          header: 'Devices',
          cell: ({ row }) => {
            const kinds = row.original.device_filter.allowed_kinds;
            // An empty filter accepts anything. Saying so beats an empty cell,
            // which reads as "nothing allowed".
            if (kinds.length === 0) {
              return <span className="type-meta-subtle">Any kind</span>;
            }
            return (
              <div className="flex flex-wrap gap-1">
                {kinds.map((kind) => (
                  <Badge key={kind} tone="neutral">
                    {kind}
                  </Badge>
                ))}
              </div>
            );
          },
        }),
        column.accessor((row) => row.minimum_free_space_percent, {
          id: 'reserve',
          header: 'Reserved',
          cell: ({ getValue }) => (
            <span className="tabular-nums text-xs">
              {getValue() === 0 ? '—' : `${getValue()}%`}
            </span>
          ),
        }),
      ]),
    [],
  );

  return (
    <>
      <PageHeader
        title="Storage classes"
        description="What each class name means: which devices may hold the data, how many copies, what they are separated across, and how much space is held back."
      />

      <Card>
        {classes.isError ? (
          <ErrorState error={classes.error} onRetry={() => void classes.refetch()} />
        ) : (
          <DataTable
            data={classes.data ?? []}
            columns={columns}
            rowId={(policy) => policy.class}
            loading={classes.isPending}
            empty={
              <EmptyState
                title="No storage classes"
                description="Every cluster has a default class even when none is configured."
              />
            }
          />
        )}
      </Card>

      <p className="type-meta-subtle">
        Classes are defined with <code>record-store storage-class set</code>. A bucket chooses one
        when it is created, and buckets that choose none use the default class.
      </p>
    </>
  );
}
