import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { ConfirmDialog } from './confirm-dialog';
import { renderWithProviders } from '@/test/render';

describe('ConfirmDialog', () => {
  it('confirms a routine action with a single click', async () => {
    const onConfirm = vi.fn();
    renderWithProviders(
      <ConfirmDialog
        open
        onOpenChange={vi.fn()}
        title="Delete object?"
        description="The current version is deleted."
        confirmLabel="Delete object"
        onConfirm={onConfirm}
      />,
    );

    await userEvent.click(screen.getByRole('button', { name: 'Delete object' }));
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('requires the exact identifier for an irreversible action', async () => {
    const onConfirm = vi.fn();
    renderWithProviders(
      <ConfirmDialog
        open
        onOpenChange={vi.fn()}
        strength="type-to-confirm"
        expectedText="storage-03"
        title="Decommission node?"
        description="Permanently removed."
        confirmLabel="Decommission node"
        onConfirm={onConfirm}
      />,
    );

    const confirm = screen.getByRole('button', { name: 'Decommission node' });
    expect(confirm.hasAttribute('disabled')).toBe(true);

    const input = screen.getByLabelText('Type storage-03 to confirm');
    await userEvent.type(input, 'storage-0');
    expect(confirm.hasAttribute('disabled')).toBe(true);

    await userEvent.type(input, '3');
    expect(confirm.hasAttribute('disabled')).toBe(false);
    await userEvent.click(confirm);
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('shows the consequence of the action', () => {
    renderWithProviders(
      <ConfirmDialog
        open
        onOpenChange={vi.fn()}
        title="Drain node?"
        description="Replicas move."
        consequence="12 replicas will be copied elsewhere first."
        confirmLabel="Start drain"
        onConfirm={vi.fn()}
      />,
    );
    expect(screen.getByText('12 replicas will be copied elsewhere first.')).toBeTruthy();
  });

  it('blocks confirmation while the request is in flight', () => {
    renderWithProviders(
      <ConfirmDialog
        open
        onOpenChange={vi.fn()}
        pending
        title="Delete?"
        description="d"
        confirmLabel="Delete"
        onConfirm={vi.fn()}
      />,
    );
    expect(screen.getByRole('button', { name: 'Working…' }).hasAttribute('disabled')).toBe(true);
  });

  it('starts unconfirmed each time it opens', async () => {
    const { rerender } = renderWithProviders(
      <ConfirmDialog
        open
        onOpenChange={vi.fn()}
        strength="type-to-confirm"
        expectedText="abc"
        title="Delete?"
        description="d"
        confirmLabel="Delete"
        onConfirm={vi.fn()}
      />,
    );
    await userEvent.type(screen.getByLabelText('Type abc to confirm'), 'abc');
    expect(screen.getByRole('button', { name: 'Delete' }).hasAttribute('disabled')).toBe(false);

    // Closing and reopening must not carry the previous confirmation over.
    rerender(
      <ConfirmDialog
        open={false}
        onOpenChange={vi.fn()}
        strength="type-to-confirm"
        expectedText="abc"
        title="Delete?"
        description="d"
        confirmLabel="Delete"
        onConfirm={vi.fn()}
      />,
    );
    rerender(
      <ConfirmDialog
        open
        onOpenChange={vi.fn()}
        strength="type-to-confirm"
        expectedText="abc"
        title="Delete?"
        description="d"
        confirmLabel="Delete"
        onConfirm={vi.fn()}
      />,
    );
    expect(screen.getByRole('button', { name: 'Delete' }).hasAttribute('disabled')).toBe(true);
  });
});
