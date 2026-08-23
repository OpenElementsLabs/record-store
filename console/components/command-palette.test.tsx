import { Circle } from 'lucide-react';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { CommandPalette, CommandTrigger } from './command-palette';
import { auditorPermissions, renderWithProviders, session } from '@/test/render';
import type { NavSection } from '@/features/system/navigation';

const push = vi.fn();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace: vi.fn() }),
}));

const sections: readonly NavSection[] = [
  { title: 'Overview', items: [{ href: '/', label: 'Overview', icon: Circle }] },
  { title: 'Storage', items: [{ href: '/buckets', label: 'Buckets', icon: Circle }] },
  {
    title: 'Access',
    items: [{ href: '/service-accounts', label: 'Service accounts', icon: Circle }],
  },
];

beforeEach(() => push.mockClear());
afterEach(() => vi.clearAllMocks());

/**
 * The palette is controlled by the shell, so tests drive it open directly.
 * Opening it by keyboard is the shell's behaviour and is covered there.
 */
async function openPalette() {
  renderWithProviders(<CommandPalette sections={sections} open onOpenChange={() => {}} />);
  return screen.findByRole('dialog');
}

describe('CommandPalette', () => {
  it('renders nothing while closed', () => {
    renderWithProviders(
      <CommandPalette sections={sections} open={false} onOpenChange={() => {}} />,
    );
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('offers a search input when open', async () => {
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
    renderWithProviders(<CommandPalette sections={sections} open onOpenChange={() => {}} />, {
      session: session(auditorPermissions),
    });
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

describe('CommandTrigger', () => {
  it('prints the shortcut so the palette is discoverable', () => {
    renderWithProviders(<CommandTrigger onOpen={() => {}} />);

    // A shortcut nobody is told about is not a feature.
    const trigger = screen.getByRole('button', { name: /Search/ });
    expect(trigger.textContent).toMatch(/K$/);
  });

  it('opens the palette when clicked', async () => {
    const onOpen = vi.fn();
    renderWithProviders(<CommandTrigger onOpen={onOpen} />);

    await userEvent.click(screen.getByRole('button', { name: /Search/ }));
    expect(onOpen).toHaveBeenCalledTimes(1);
  });
});
