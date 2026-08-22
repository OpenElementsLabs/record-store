'use client';

import {
  createSortedRowModel,
  rowSortingFeature,
  sortFn_alphanumeric,
  sortFn_basic,
  sortFn_datetime,
  sortFn_text,
  tableFeatures,
  useTable,
  type RowData,
  type SortDirection,
  type SortingState,
  type TableOptions,
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

/**
 * The feature set every console table is built from.
 *
 * Only sorting is registered, and that is deliberate. Without
 * `rowPaginationFeature` the table has no way to slice its input, so a screen
 * that pages on the server cannot accidentally end up paginating one loaded
 * page in the browser. Filtering is likewise absent: the one screen that
 * filters narrows the array it passes in, so the rows on screen are always the
 * rows the screen asked for.
 */
export const dataTableFeatures = tableFeatures({
  rowSortingFeature,
  sortedRowModel: createSortedRowModel(),
  // Registered individually rather than through the whole `sortFns` registry so
  // only these comparators are bundled. `sortFn: 'auto'` resolves among them.
  sortFns: {
    alphanumeric: sortFn_alphanumeric,
    basic: sortFn_basic,
    datetime: sortFn_datetime,
    text: sortFn_text,
  },
});

/** The concrete feature registry, for typing columns against this table. */
export type DataTableFeatures = typeof dataTableFeatures;

/**
 * Columns for the shared table.
 *
 * Build these with `createColumnHelper<DataTableFeatures, TRow>()` so each
 * accessor keeps its own value type instead of widening to a single one.
 */
export type DataTableColumns<TData extends RowData> = TableOptions<
  DataTableFeatures,
  TData
>['columns'];

export type DataTableProps<TData extends RowData> = {
  /** One page of rows, already in the order they should appear. */
  readonly data: readonly TData[];
  readonly columns: DataTableColumns<TData>;
  /** Stable row identity, so rows are not re-keyed by index on reorder. */
  readonly rowId: (row: TData) => string;
  readonly loading?: boolean;
  readonly empty?: React.ReactNode;
  readonly onRowClick?: (row: TData) => void;
  readonly initialSorting?: SortingState;
  /**
   * Set when `data` is one server-ordered page. Client sorting is then skipped
   * rather than reordering the loaded page, which would look like sorting the
   * whole dataset while only touching the rows in hand.
   */
  readonly serverOrdered?: boolean;
};

/** A stable empty page, so a missing query result does not invalidate the row model. */
const NO_ROWS: readonly never[] = [];

/**
 * The shared table for operational lists.
 *
 * It renders exactly the rows it is given. Client-side sorting serves the
 * bounded lists that use it — buckets, service accounts, nodes — and is turned
 * off with `serverOrdered` for cursor-paged screens, where order belongs to the
 * backend.
 */
export function DataTable<TData extends RowData>({
  data,
  columns,
  rowId,
  loading = false,
  empty,
  onRowClick,
  initialSorting = [],
  serverOrdered = false,
}: DataTableProps<TData>) {
  const [sorting, setSorting] = React.useState<SortingState>(initialSorting);

  const table = useTable({
    features: dataTableFeatures,
    columns,
    data: data.length > 0 ? data : NO_ROWS,
    state: { sorting },
    onSortingChange: setSorting,
    getRowId: (row) => rowId(row),
    manualSorting: serverOrdered,
    enableSorting: !serverOrdered,
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
                        <table.FlexRender header={header} />
                        {direction === 'asc' ? (
                          <ArrowUp aria-hidden className="size-3" />
                        ) : direction === 'desc' ? (
                          <ArrowDown aria-hidden className="size-3" />
                        ) : (
                          <ChevronsUpDown aria-hidden className="size-3 opacity-50" />
                        )}
                      </button>
                    ) : (
                      <table.FlexRender header={header} />
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
              {row.getAllCells().map((cell) => (
                <TableCell key={cell.id}>
                  <table.FlexRender cell={cell} />
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </TableShell>
  );
}

function ariaSort(direction: false | SortDirection): 'ascending' | 'descending' | undefined {
  if (direction === 'asc') return 'ascending';
  if (direction === 'desc') return 'descending';
  return undefined;
}
