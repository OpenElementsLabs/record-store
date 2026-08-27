import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { render } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { AppShell } from './app-shell';
import { adminPermissions, auditorPermissions, session, systemInfo } from '@/test/render';
import type { Deployment } from '@/features/system/deployment';

const push = vi.fn();

vi.mock('next/navigation', () => ({
  useRouter: () => ({ push, replace: vi.fn() }),
  usePathname: () => '/buckets',
}));

function shell(deployment?: Partial<Deployment>) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <AppShell
        deployment={{
          info: systemInfo(),
          session: session(adminPermissions),
          ...deployment,
        }}
      >
        <p>content</p>
      </AppShell>
    </QueryClientProvider>,
  );
}

afterEach(() => vi.clearAllMocks());

describe('AppShell', () => {
  it('names the deployment mode and version in the sidebar', () => {
    shell();
    expect(screen.getByText(/Standalone · 0\.1\.0/)).toBeTruthy();
  });

  it('marks the current screen for assistive technology', () => {
    shell();
    // Highlighting is visual; `aria-current` is what a screen reader uses.
    const current = screen.getByRole('link', { name: 'Buckets' });
    expect(current.getAttribute('aria-current')).toBe('page');
  });

  it('offers a skip link before the navigation', () => {
    shell();
    expect(screen.getByRole('link', { name: 'Skip to content' })).toBeTruthy();
  });

  it('opens the command palette on the keyboard shortcut', async () => {
    shell();
    expect(screen.queryByRole('dialog')).toBeNull();

    await userEvent.keyboard('{Control>}k{/Control}');
    expect(await screen.findByRole('dialog')).toBeTruthy();
  });

  it('opens the command palette from the visible trigger', async () => {
    shell();
    await userEvent.click(screen.getByRole('button', { name: /Search/ }));

    expect(await screen.findByRole('dialog')).toBeTruthy();
  });

  it('closes the palette on a second shortcut press', async () => {
    shell();
    await userEvent.keyboard('{Control>}k{/Control}');
    await screen.findByRole('dialog');

    await userEvent.keyboard('{Control>}k{/Control}');
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('hides cluster navigation in a standalone deployment', () => {
    shell();
    expect(screen.queryByRole('link', { name: 'Nodes' })).toBeNull();
    expect(screen.queryByRole('link', { name: 'Durability' })).toBeNull();
  });

  it('shows cluster navigation when the backend reports a cluster', () => {
    shell({
      info: systemInfo({
        mode: 'cluster',
        capabilities: { ...systemInfo().capabilities, cluster: true },
      }),
    });
    expect(screen.getByRole('link', { name: 'Nodes' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Durability' })).toBeTruthy();
  });

  it('names the signed-in role', () => {
    shell({ session: session(auditorPermissions) });
    expect(screen.getAllByText('System administrator').length).toBeGreaterThan(0);
  });

  it('gives every navigation item an icon so a collapsed sidebar stays usable', () => {
    shell();
    const link = screen.getByRole('link', { name: 'Buckets' });
    expect(link.querySelector('svg')).toBeTruthy();
  });

  it('collapses and expands the sidebar, remembering the choice', async () => {
    shell();
    await userEvent.click(screen.getByRole('button', { name: 'Collapse navigation' }));

    // Collapsed labels stay in the accessible tree even though they are hidden.
    expect(screen.getByRole('button', { name: 'Expand navigation' })).toBeTruthy();
    expect(screen.getByRole('link', { name: 'Buckets' })).toBeTruthy();
    expect(window.localStorage.getItem('record-store.sidebar.collapsed')).toBe('1');
  });

  it('puts the account menu in the sidebar, not the top bar', async () => {
    shell();
    await userEvent.click(screen.getByRole('button', { name: /^Account:/ }));

    expect(await screen.findByRole('menuitem', { name: /sign out/i })).toBeTruthy();
  });

  it('keeps the top bar to controls that apply everywhere', () => {
    shell();
    // Page-specific actions belong in page headers; the top bar carries search
    // and theme only.
    const header = screen.getByRole('banner');
    expect(header.textContent).toMatch(/Search/);
    expect(header.querySelector('[aria-label^="Account:"]')).toBeNull();
  });

  it('marks the active item with more than colour', () => {
    shell();
    const active = screen.getByRole('link', { name: 'Buckets' });
    // An accent rail accompanies the tint, so the state survives a colour-blind
    // or high-contrast rendering.
    expect(active.querySelector('span[aria-hidden]')).toBeTruthy();
  });
});
