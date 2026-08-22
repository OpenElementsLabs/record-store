'use client';

import {
  type ColumnDef,
  type SortingState,
  flexRender,
  getCoreRowModel,
  getSortedRowModel,
  useReactTable,
} from '@tanstack/react-table';
import { ArrowDown, ArrowUp, ChevronsUpDown } from 'lucide-react';
import * as React from 'react';

import { EmptyState } from '@/components/empty-state';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
  TableShell,
} from '@/components/ui/table';
import { TableSkeleton } from '@/components/ui/skeleton';
import { cn } from '@/lib/utils';

export type DataTableProps<TData> = {
  readonly data: readonly TData[];
  readonly columns: readonly ColumnDef<TData, unknown>[];
  /** Stable row identity, so rows are not re-keyed by index on reorder. */
  readonly rowId: (row: TData) => string;
  readonly loading?: boolean;
  readonly empty?: React.ReactNode;
  readonly onRowClick?: (row: TData) => void;
  readonly initialSorting?: SortingState;
};

/**
 * The shared table for operational lists.
 *
 * Sorting is client side, which is appropriate for the bounded lists that use it
 * (buckets, accounts, policies, nodes). Screens whose data is genuinely large —
 * objects, audit, events — page on the server and pass one page at a time.
 */
export function DataTable<TData>({
  data,
  columns,
  rowId,
  loading = false,
  empty,
  onRowClick,
  initialSorting = [],
}: DataTableProps<TData>) {
  const [sorting, setSorting] = React.useState<SortingState>(initialSorting);

  // The table library is not React Compiler compatible, so this component is
  // skipped by the compiler. Behaviour is unaffected; only auto-memoisation is.
  // eslint-disable-next-line react-hooks/incompatible-library
  const table = useReactTable({
    data: data as TData[],
    columns: columns as ColumnDef<TData, unknown>[],
    state: { sorting },
    onSortingChange: setSorting,
    getRowId: (row) => rowId(row),
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
  });

  if (loading && data.length === 0) {
    return <TableSkeleton columns={Math.min(columns.length, 6)} />;
  }
  if (data.length === 0) {
    return (
      <>{empty ?? <EmptyState title="Nothing to show" description="There is no data yet." />}</>
    );
  }

  return (
    <TableShell>
      <Table>
        <TableHeader>
          {table.getHeaderGroups().map((headerGroup) => (
            <TableRow key={headerGroup.id} className="hover:bg-transparent">
              {headerGroup.headers.map((header) => {
                const sortable = header.column.getCanSort();
                const direction = header.column.getIsSorted();
                return (
                  <TableHead key={header.id} aria-sort={ariaSort(direction)}>
                    {header.isPlaceholder ? null : sortable ? (
                      <button
                        type="button"
                        onClick={header.column.getToggleSortingHandler()}
                        className="inline-flex items-center gap-1 hover:text-ink"
                      >
                        {flexRender(header.column.columnDef.header, header.getContext())}
                        {direction === 'asc' ? (
                          <ArrowUp aria-hidden className="size-3" />
                        ) : direction === 'desc' ? (
                          <ArrowDown aria-hidden className="size-3" />
                        ) : (
                          <ChevronsUpDown aria-hidden className="size-3 opacity-50" />
                        )}
                      </button>
                    ) : (
                      flexRender(header.column.columnDef.header, header.getContext())
                    )}
                  </TableHead>
                );
              })}
            </TableRow>
          ))}
        </TableHeader>
        <TableBody>
          {table.getRowModel().rows.map((row) => (
            <TableRow
              key={row.id}
              className={cn(onRowClick && 'cursor-pointer')}
              onClick={onRowClick ? () => onRowClick(row.original) : undefined}
            >
              {row.getVisibleCells().map((cell) => (
                <TableCell key={cell.id}>
                  {flexRender(cell.column.columnDef.cell, cell.getContext())}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </TableShell>
  );
}

function ariaSort(direction: false | 'asc' | 'desc'): 'ascending' | 'descending' | undefined {
  if (direction === 'asc') return 'ascending';
  if (direction === 'desc') return 'descending';
  return undefined;
}
