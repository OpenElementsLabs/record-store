'use client';

import { LogOut, Menu, X } from 'lucide-react';
import Link from 'next/link';
import { usePathname, useRouter } from 'next/navigation';
import * as React from 'react';

import { CommandPalette, CommandTrigger } from '@/components/command-palette';
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
  const [paletteOpen, setPaletteOpen] = React.useState(false);
  const [collapsed, setCollapsed] = React.useState<boolean>(() => {
    if (typeof window === 'undefined') return false;
    return window.localStorage.getItem('oes-sidebar-collapsed') === 'true';
  });

  React.useEffect(() => {
    if (typeof window !== 'undefined') {
      window.localStorage.setItem('oes-sidebar-collapsed', String(collapsed));
    }
  }, [collapsed]);

  // The shortcut lives with the state it toggles rather than inside the dialog,
  // so the visible trigger and the keyboard both drive one source of truth.
  React.useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault();
        setPaletteOpen((current) => !current);
      }
    }
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, []);

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
      <CommandPalette sections={sections} open={paletteOpen} onOpenChange={setPaletteOpen} />
      <div
        className="min-h-screen lg:grid"
        style={{
          gridTemplateColumns: collapsed ? '4.5rem minmax(0, 1fr)' : '15rem minmax(0, 1fr)',
        }}
      >
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
          collapsed={collapsed}
          onToggleCollapsed={() => setCollapsed((current) => !current)}
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
              collapsed={false}
              className="relative flex h-full w-72 max-w-[80vw]"
              onNavigate={() => setMobileOpen(false)}
            />
          </div>
        ) : null}

        <div className="flex min-w-0 flex-col">
          <TopBar
            deployment={deployment}
            onOpenNavigation={() => setMobileOpen(true)}
            mobileOpen={mobileOpen}
            onOpenPalette={() => setPaletteOpen(true)}
            onToggleSidebar={() => setCollapsed((current) => !current)}
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
  collapsed,
  className,
  onNavigate,
  onToggleCollapsed,
}: {
  readonly sections: ReturnType<typeof buildNavigation>;
  readonly pathname: string;
  readonly deployment: Deployment;
  readonly collapsed?: boolean;
  readonly className?: string;
  readonly onNavigate?: () => void;
  readonly onToggleCollapsed?: () => void;
}) {
  return (
    <nav
      aria-label="Console sections"
      className={cn(
        'flex-col gap-5 overflow-y-auto border-r border-border bg-surface/90 px-3 py-4 shadow-[inset_-1px_0_0_rgba(148,163,184,0.12)] backdrop-blur lg:sticky lg:top-0 lg:h-screen',
        collapsed ? 'items-center px-2' : 'items-stretch',
        className,
      )}
    >
      <div className={cn('flex items-center gap-2 px-2', collapsed && 'justify-center')}>
        <Link
          href="/"
          className={cn(
            'flex items-center gap-2 rounded-[--radius-control] px-1.5 py-1.5 text-left hover:bg-surface-muted',
            collapsed ? 'justify-center' : '',
          )}
          onClick={onNavigate}
          aria-label="OES Console"
          title="OES Console"
        >
          <span className="flex size-6 items-center justify-center rounded-md bg-accent text-xs font-semibold text-accent-ink">
            O
          </span>
          {!collapsed ? (
            <span className="flex items-baseline gap-1.5">
              <span className="text-base font-semibold tracking-tight text-ink">OES</span>
              <span className="text-[0.65rem] uppercase tracking-[0.12em] text-ink-subtle">
                Console
              </span>
            </span>
          ) : null}
        </Link>
        {!collapsed && onToggleCollapsed ? (
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="ml-auto size-8"
            aria-label="Collapse navigation"
            onClick={onToggleCollapsed}
          >
            <X aria-hidden className="size-4" />
          </Button>
        ) : null}
      </div>

      {!collapsed ? (
        <p className="px-2 text-[0.625rem] font-medium uppercase tracking-[0.14em] text-ink-subtle">
          {deployment.info.mode === 'cluster' ? 'Cluster' : 'Standalone'} ·{' '}
          {deployment.info.version}
        </p>
      ) : null}

      <div className="flex flex-col gap-5">
        {sections.map((section) => (
          <div key={section.title} className={cn('space-y-1', collapsed && 'w-full')}>
            {!collapsed ? (
              <p className="px-2 text-[0.6875rem] font-medium uppercase tracking-[0.14em] text-ink-subtle">
                {section.title}
              </p>
            ) : null}
            <ul className={cn('space-y-0.5', collapsed && 'flex flex-col items-center')}>
              {section.items.map((item) => {
                const active = isActive(item, pathname);
                const shortLabel = item.label
                  .split(/\s+/)
                  .map((part) => part[0])
                  .join('')
                  .slice(0, 2)
                  .toUpperCase();
                return (
                  <li key={item.href} className="w-full">
                    <Link
                      href={item.href}
                      onClick={onNavigate}
                      aria-current={active ? 'page' : undefined}
                      aria-label={item.label}
                      title={collapsed ? item.label : undefined}
                      className={cn(
                        'flex items-center rounded-[--radius-control] text-sm transition-colors',
                        collapsed ? 'h-10 w-10 justify-center self-center' : 'gap-2 px-2 py-1.5',
                        active
                          ? 'bg-accent-soft font-medium text-accent ring-1 ring-inset ring-accent/20'
                          : 'text-ink-muted hover:bg-surface-muted hover:text-ink',
                      )}
                    >
                      <span
                        className={cn(
                          'flex size-5 items-center justify-center rounded-md text-[0.55rem] font-semibold',
                          active ? 'bg-accent text-accent-ink' : 'bg-surface-muted text-ink-subtle',
                        )}
                      >
                        {collapsed ? shortLabel || '•' : item.label.slice(0, 1).toUpperCase()}
                      </span>
                      {!collapsed ? <span className="truncate">{item.label}</span> : null}
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
  onOpenPalette,
  onToggleSidebar,
}: {
  readonly deployment: Deployment;
  readonly onOpenNavigation: () => void;
  readonly mobileOpen: boolean;
  readonly onOpenPalette: () => void;
  readonly onToggleSidebar?: () => void;
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
    <header className="sticky top-0 z-30 flex h-14 items-center justify-between gap-3 border-b border-border bg-surface/90 px-4 backdrop-blur-sm sm:px-6">
      <div className="flex items-center gap-2">
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
        {onToggleSidebar ? (
          <Button
            variant="ghost"
            size="icon"
            className="hidden lg:inline-flex"
            onClick={onToggleSidebar}
            aria-label="Toggle navigation"
          >
            <Menu aria-hidden />
          </Button>
        ) : null}
      </div>

      <div className="min-w-0 flex-1">
        <CommandTrigger onOpen={onOpenPalette} />
      </div>

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
