import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { CommandPalette } from './command-palette';
import { auditorPermissions, renderWithProviders, session } from '@/test/render';
import type { NavSection } from '@/features/system/navigation';

const push = vi.fn();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace: vi.fn() }),
}));

const sections: readonly NavSection[] = [
  { title: 'Overview', items: [{ href: '/', label: 'Overview' }] },
  { title: 'Storage', items: [{ href: '/buckets', label: 'Buckets' }] },
  { title: 'Access', items: [{ href: '/service-accounts', label: 'Service accounts' }] },
];

beforeEach(() => push.mockClear());
afterEach(() => vi.clearAllMocks());

async function openPalette() {
  renderWithProviders(<CommandPalette sections={sections} />);
  await userEvent.keyboard('{Meta>}k{/Meta}');
  return screen.findByRole('dialog');
}

describe('CommandPalette', () => {
  it('stays closed until the shortcut is pressed', () => {
    renderWithProviders(<CommandPalette sections={sections} />);
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('opens on the keyboard shortcut', async () => {
    await openPalette();
    expect(screen.getByLabelText('Search commands')).toBeTruthy();
  });

  it('narrows the list as the operator types', async () => {
    await openPalette();
    await userEvent.type(screen.getByLabelText('Search commands'), 'sacc');

    const labels = screen.getAllByRole('option').map((option) => option.textContent ?? '');
    // Both the screen and the create action legitimately contain this
    // subsequence; what matters is that unrelated commands are gone.
    expect(labels.some((label) => label.includes('Service accounts'))).toBe(true);
    expect(labels.some((label) => label.includes('Buckets'))).toBe(false);
    expect(labels.some((label) => label.includes('Overview'))).toBe(false);
  });

  it('navigates on Enter to the highlighted command', async () => {
    await openPalette();
    await userEvent.type(screen.getByLabelText('Search commands'), 'buckets{Enter}');

    expect(push).toHaveBeenCalledWith('/buckets');
  });

  it('moves the highlight with the arrow keys', async () => {
    await openPalette();
    const input = screen.getByLabelText('Search commands');
    await userEvent.type(input, '{ArrowDown}{ArrowDown}{Enter}');

    // Third command in the list, since the highlight starts on the first.
    expect(push).toHaveBeenCalledWith('/service-accounts');
  });

  it('does not run past the end of the list', async () => {
    await openPalette();
    const input = screen.getByLabelText('Search commands');
    // Far more presses than there are commands: the highlight must clamp to the
    // last one rather than running off the end and selecting nothing.
    await userEvent.type(input, '{ArrowDown>12/}{Enter}');

    // Create policy is last for an administrator; the palette closes on Enter,
    // so the assertion is on where it navigated.
    expect(push).toHaveBeenCalledWith('/policies?create=1');
  });

  it('says plainly when nothing matches', async () => {
    await openPalette();
    await userEvent.type(screen.getByLabelText('Search commands'), 'zzzz');

    expect(screen.getByText(/Nothing matches/)).toBeTruthy();
    expect(screen.queryAllByRole('option')).toHaveLength(0);
  });

  it('offers no action an auditor cannot perform', async () => {
    renderWithProviders(<CommandPalette sections={sections} />, {
      session: session(auditorPermissions),
    });
    await userEvent.keyboard('{Control>}k{/Control}');
    await screen.findByRole('dialog');

    // The palette must never be a route around role gating.
    expect(screen.queryByText('Create bucket')).toBeNull();
    expect(screen.queryByText('Create service account')).toBeNull();
  });

  it('offers creation shortcuts to a role that can create', async () => {
    await openPalette();
    await userEvent.type(screen.getByLabelText('Search commands'), 'create');

    const labels = screen.getAllByRole('option').map((option) => option.textContent ?? '');
    expect(labels.some((label) => label.includes('Create bucket'))).toBe(true);
  });
});
