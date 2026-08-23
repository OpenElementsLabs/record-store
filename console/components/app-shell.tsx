'use client';

import { LogOut, Menu, X } from 'lucide-react';
import Link from 'next/link';
import { usePathname, useRouter } from 'next/navigation';
import * as React from 'react';

import { CommandPalette } from '@/components/command-palette';
import { ThemeToggle } from '@/components/theme-toggle';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { DeploymentProvider, type Deployment } from '@/features/system/deployment';
import { buildNavigation, isActive } from '@/features/system/navigation';
import { cn } from '@/lib/utils';

const ROLE_LABEL: Record<string, string> = {
  system_administrator: 'System administrator',
  storage_administrator: 'Storage administrator',
  auditor: 'Auditor',
};

/**
 * The authenticated application frame.
 *
 * Navigation is derived once from the deployment and role, so individual screens
 * never repeat mode or permission checks just to decide whether they belong.
 */
export function AppShell({
  deployment,
  children,
}: {
  readonly deployment: Deployment;
  readonly children: React.ReactNode;
}) {
  const pathname = usePathname();
  const [mobileOpen, setMobileOpen] = React.useState(false);

  const sections = React.useMemo(
    () =>
      buildNavigation({
        clusterEnabled: deployment.info.mode === 'cluster' && deployment.info.capabilities.cluster,
        capabilities: deployment.info.capabilities,
        permissions: deployment.session.permissions,
      }),
    [deployment],
  );

  return (
    <DeploymentProvider value={deployment}>
      {/*
        Inside the provider so the palette sees the same role and capabilities
        the sidebar does, and cannot offer a screen the sidebar hides.
      */}
      <CommandPalette sections={sections} />
      <div className="min-h-screen lg:grid lg:grid-cols-[15rem_1fr]">
        <a
          href="#main"
          className="sr-only focus:not-sr-only focus:absolute focus:left-2 focus:top-2 focus:z-50 focus:rounded focus:bg-surface focus:px-3 focus:py-2 focus:text-sm"
        >
          Skip to content
        </a>

        <Sidebar
          sections={sections}
          pathname={pathname}
          deployment={deployment}
          className="hidden lg:flex"
        />

        {mobileOpen ? (
          <div className="fixed inset-0 z-40 lg:hidden">
            <button
              type="button"
              aria-label="Close navigation"
              className="absolute inset-0 bg-black/40"
              onClick={() => setMobileOpen(false)}
            />
            <Sidebar
              sections={sections}
              pathname={pathname}
              deployment={deployment}
              className="relative flex h-full w-64 max-w-[80vw]"
              // Following a link on a phone should dismiss the drawer. Doing it
              // from the click keeps it out of a render effect.
              onNavigate={() => setMobileOpen(false)}
            />
          </div>
        ) : null}

        <div className="flex min-w-0 flex-col">
          <TopBar
            deployment={deployment}
            onOpenNavigation={() => setMobileOpen(true)}
            mobileOpen={mobileOpen}
          />
          <main id="main" className="min-w-0 flex-1 px-4 py-6 sm:px-6">
            <div className="mx-auto max-w-7xl space-y-6">{children}</div>
          </main>
        </div>
      </div>
    </DeploymentProvider>
  );
}

function Sidebar({
  sections,
  pathname,
  deployment,
  className,
  onNavigate,
}: {
  readonly sections: ReturnType<typeof buildNavigation>;
  readonly pathname: string;
  readonly deployment: Deployment;
  readonly className?: string;
  readonly onNavigate?: () => void;
}) {
  return (
    <nav
      aria-label="Console sections"
      className={cn(
        'flex-col gap-6 overflow-y-auto border-r border-border bg-surface px-3 py-4 lg:sticky lg:top-0 lg:h-screen',
        className,
      )}
    >
      <div className="px-2">
        <Link href="/" className="flex items-baseline gap-2" onClick={onNavigate}>
          <span className="text-base font-semibold tracking-tight text-ink">OES</span>
          <span className="text-xs text-ink-subtle">Console</span>
        </Link>
        <p className="mt-1 text-xs text-ink-subtle">
          {deployment.info.mode === 'cluster' ? 'Cluster' : 'Standalone'} ·{' '}
          {deployment.info.version}
        </p>
      </div>

      <div className="flex flex-col gap-5">
        {sections.map((section) => (
          <div key={section.title} className="space-y-1">
            <p className="px-2 text-[0.6875rem] font-medium uppercase tracking-wide text-ink-subtle">
              {section.title}
            </p>
            <ul className="space-y-0.5">
              {section.items.map((item) => {
                const active = isActive(item, pathname);
                return (
                  <li key={item.href}>
                    <Link
                      href={item.href}
                      onClick={onNavigate}
                      aria-current={active ? 'page' : undefined}
                      className={cn(
                        'block rounded-[--radius-control] px-2 py-1.5 text-sm',
                        active
                          ? 'bg-accent-soft font-medium text-accent'
                          : 'text-ink-muted hover:bg-surface-muted hover:text-ink',
                      )}
                    >
                      {item.label}
                    </Link>
                  </li>
                );
              })}
            </ul>
          </div>
        ))}
      </div>
    </nav>
  );
}

function TopBar({
  deployment,
  onOpenNavigation,
  mobileOpen,
}: {
  readonly deployment: Deployment;
  readonly onOpenNavigation: () => void;
  readonly mobileOpen: boolean;
}) {
  const router = useRouter();
  const [signingOut, setSigningOut] = React.useState(false);

  async function signOut() {
    setSigningOut(true);
    try {
      await fetch('/api/auth/logout', { method: 'POST', credentials: 'same-origin' });
    } finally {
      router.replace('/login');
    }
  }

  return (
    <header className="sticky top-0 z-30 flex h-14 items-center justify-between gap-3 border-b border-border bg-surface/95 px-4 backdrop-blur sm:px-6">
      <Button
        variant="ghost"
        size="icon"
        className="lg:hidden"
        onClick={onOpenNavigation}
        aria-label="Open navigation"
        aria-expanded={mobileOpen}
      >
        {mobileOpen ? <X aria-hidden /> : <Menu aria-hidden />}
      </Button>

      <div className="min-w-0 flex-1" />

      <div className="flex items-center gap-2">
        <ThemeToggle />
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="secondary" size="sm">
              <span className="max-w-40 truncate">
                {ROLE_LABEL[deployment.session.role] ?? deployment.session.role}
              </span>
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            <DropdownMenuLabel>Signed in</DropdownMenuLabel>
            <div className="px-2 pb-2">
              <Badge tone="accent">
                {ROLE_LABEL[deployment.session.role] ?? deployment.session.role}
              </Badge>
            </div>
            <DropdownMenuSeparator />
            <DropdownMenuItem onSelect={signOut} disabled={signingOut}>
              <LogOut aria-hidden />
              {signingOut ? 'Signing out…' : 'Sign out'}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </header>
  );
}
