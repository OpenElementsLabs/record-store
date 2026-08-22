import { createColumnHelper } from '@tanstack/react-table';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { DataTable, type DataTableFeatures } from './data-table';
import { EmptyState } from './empty-state';

type Row = {
  readonly id: string;
  readonly name: string;
  readonly bytes: number;
  readonly created: string;
};

const column = createColumnHelper<DataTableFeatures, Row>();

/**
 * Columns are typed against `Row`, so a cell reads a real field of the model
 * rather than an untyped bag of values.
 */
const columns = column.columns([
  column.accessor('name', { header: 'Name' }),
  column.accessor('bytes', {
    header: 'Size',
    cell: ({ getValue }) => <span data-testid="size">{getValue().toLocaleString('en-GB')}</span>,
  }),
  column.accessor('created', { header: 'Created' }),
  column.display({
    id: 'actions',
    header: () => <span className="sr-only">Actions</span>,
    cell: ({ row }) => <button type="button">Delete {row.original.name}</button>,
  }),
]);

function rows(count: number): Row[] {
  return Array.from({ length: count }, (_, index) => ({
    id: `id-${index}`,
    name: `object-${String(count - index).padStart(3, '0')}`,
    bytes: (index + 1) * 1_000,
    created: `2026-08-${String((index % 28) + 1).padStart(2, '0')}T00:00:00Z`,
  }));
}

function bodyText(): string[] {
  const table = screen.getByRole('table');
  const [, ...bodyRows] = within(table).getAllByRole('row');
  return bodyRows.map((row) => within(row).getAllByRole('cell')[0]?.textContent ?? '');
}

describe('DataTable', () => {
  it('renders typed rows and cells from the model', () => {
    render(<DataTable data={rows(2)} columns={columns} rowId={(row) => row.id} />);

    expect(screen.getByText('object-002')).toBeTruthy();
    expect(screen.getAllByTestId('size').map((cell) => cell.textContent)).toEqual([
      '1,000',
      '2,000',
    ]);
    expect(screen.getByRole('button', { name: 'Delete object-002' })).toBeTruthy();
  });

  it('shows a loading placeholder before the first page arrives', () => {
    render(<DataTable data={[]} columns={columns} rowId={(row) => row.id} loading />);

    expect(screen.getByRole('status', { name: 'Loading' })).toBeTruthy();
    expect(screen.queryByRole('table')).toBeNull();
  });

  it('shows the caller’s empty state rather than an empty grid', () => {
    render(
      <DataTable
        data={[]}
        columns={columns}
        rowId={(row) => row.id}
        empty={<EmptyState title="No objects" description="Upload something first." />}
      />,
    );

    expect(screen.getByText('No objects')).toBeTruthy();
    expect(screen.queryByRole('table')).toBeNull();
  });

  it('keeps loaded rows visible while a refetch is in flight', () => {
    render(<DataTable data={rows(2)} columns={columns} rowId={(row) => row.id} loading />);

    // Loading with rows in hand is a background refresh, not a first load.
    expect(screen.getByRole('table')).toBeTruthy();
    expect(screen.queryByRole('status', { name: 'Loading' })).toBeNull();
  });

  it('sorts on the client and reports direction to assistive technology', async () => {
    render(
      <DataTable
        data={rows(3)}
        columns={columns}
        rowId={(row) => row.id}
        initialSorting={[{ id: 'name', desc: false }]}
      />,
    );

    expect(bodyText()).toEqual(['object-001', 'object-002', 'object-003']);
    const header = screen.getByRole('columnheader', { name: 'Name' });
    expect(header.getAttribute('aria-sort')).toBe('ascending');

    await userEvent.click(within(header).getByRole('button'));
    expect(bodyText()).toEqual(['object-003', 'object-002', 'object-001']);
    expect(screen.getByRole('columnheader', { name: 'Name' }).getAttribute('aria-sort')).toBe(
      'descending',
    );
  });

  it('offers no sort control on a display column', () => {
    render(<DataTable data={rows(1)} columns={columns} rowId={(row) => row.id} />);

    const header = screen.getByRole('columnheader', { name: 'Actions' });
    expect(within(header).queryByRole('button')).toBeNull();
    expect(header.getAttribute('aria-sort')).toBeNull();
  });

  it('reports the clicked row’s own model to the caller', async () => {
    const onRowClick = vi.fn();
    render(
      <DataTable
        data={rows(2)}
        columns={columns}
        rowId={(row) => row.id}
        onRowClick={onRowClick}
        initialSorting={[{ id: 'name', desc: false }]}
      />,
    );

    const table = screen.getByRole('table');
    const [, firstRow] = within(table).getAllByRole('row');
    await userEvent.click(firstRow as HTMLElement);
    expect(onRowClick).toHaveBeenCalledWith(expect.objectContaining({ name: 'object-001' }));
  });

  it('renders every row it is given, so a server page is never re-paginated', () => {
    const page = rows(137);
    render(<DataTable data={page} columns={columns} rowId={(row) => row.id} />);

    // No pagination feature is registered, so the table cannot slice its input.
    // A cursor-paged screen therefore shows exactly the page it fetched.
    const table = screen.getByRole('table');
    expect(within(table).getAllByRole('row')).toHaveLength(page.length + 1);
  });

  it('leaves a server-ordered page in the order the backend returned', async () => {
    render(
      <DataTable
        data={rows(3)}
        columns={columns}
        rowId={(row) => row.id}
        serverOrdered
        initialSorting={[{ id: 'name', desc: false }]}
      />,
    );

    // The backend chose this order and only holds one page, so the table must
    // not reorder the rows in hand and present it as a sort of the whole set.
    expect(bodyText()).toEqual(['object-003', 'object-002', 'object-001']);
    const header = screen.getByRole('columnheader', { name: 'Name' });
    expect(within(header).queryByRole('button')).toBeNull();
    await userEvent.click(header);
    expect(bodyText()).toEqual(['object-003', 'object-002', 'object-001']);
  });

  it('keys rows by their identity rather than their position', () => {
    const { rerender } = render(
      <DataTable data={rows(2)} columns={columns} rowId={(row) => row.id} />,
    );
    const first = screen.getByText('object-002');

    rerender(<DataTable data={[...rows(2)].reverse()} columns={columns} rowId={(row) => row.id} />);

    // Reordering moves the same DOM node instead of rewriting cell contents.
    expect(screen.getByText('object-002')).toBe(first);
  });
});
