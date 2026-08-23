'use client';

import { LogOut, Menu, PanelLeftClose, PanelLeftOpen, UserRound, X } from 'lucide-react';
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
  // Reading storage in a `useState` initializer diverges between the server
  // render and hydration, so the preference comes from an external store with an
  // explicit server snapshot instead.
  const [collapsed, setCollapsed] = useCollapsed();

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
          className="sr-only focus:not-sr-only focus:absolute focus:left-2 focus:top-2 focus:z-50 focus:rounded-[--radius-control] focus:bg-surface focus:px-3 focus:py-2 focus:text-sm"
        >
          Skip to content
        </a>

        <Sidebar
          sections={sections}
          pathname={pathname}
          deployment={deployment}
          collapsed={collapsed}
          onToggleCollapsed={() => setCollapsed(!collapsed)}
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
            onOpenNavigation={() => setMobileOpen(true)}
            mobileOpen={mobileOpen}
            onOpenPalette={() => setPaletteOpen(true)}
          />
          <main id="main" className="min-w-0 flex-1 px-4 py-6 sm:px-6">
            <div className="mx-auto max-w-7xl space-y-6">{children}</div>
          </main>
        </div>
      </div>
    </DeploymentProvider>
  );
}

/** Where the collapsed preference is remembered. */
const COLLAPSE_KEY = 'oes.sidebar.collapsed';

/**
 * Reads the stored collapse preference.
 *
 * Through an external store with a server snapshot of "expanded", so the markup
 * the server sends and the first client paint agree, and a private window that
 * refuses storage simply gets the default rather than throwing.
 */
function useCollapsed(): readonly [boolean, (collapsed: boolean) => void] {
  const [override, setOverride] = React.useState<boolean | null>(null);
  const stored = React.useSyncExternalStore(
    () => () => {},
    () => {
      try {
        return window.localStorage.getItem(COLLAPSE_KEY) === '1';
      } catch {
        return false;
      }
    },
    () => false,
  );
  const collapsed = override ?? stored;
  return [
    collapsed,
    (next: boolean) => {
      setOverride(next);
      try {
        window.localStorage.setItem(COLLAPSE_KEY, next ? '1' : '0');
      } catch {
        // A preference that cannot be stored is still honoured for this session.
      }
    },
  ];
}

function Sidebar({
  sections,
  pathname,
  deployment,
  className,
  onNavigate,
  collapsed = false,
  onToggleCollapsed,
}: {
  readonly sections: ReturnType<typeof buildNavigation>;
  readonly pathname: string;
  readonly deployment: Deployment;
  readonly className?: string;
  readonly onNavigate?: () => void;
  readonly collapsed?: boolean;
  readonly onToggleCollapsed?: () => void;
}) {
  return (
    <nav
      aria-label="Console sections"
      className={cn(
        'flex-col overflow-y-auto border-r border-border bg-surface lg:sticky lg:top-0 lg:h-screen',
        collapsed ? 'px-2 py-3' : 'px-3 py-3',
        className,
      )}
    >
      <div className={cn('flex items-center gap-2', collapsed ? 'justify-center' : 'px-2')}>
        <Link
          href="/"
          onClick={onNavigate}
          className="flex min-w-0 items-baseline gap-2"
          aria-label="OES console home"
        >
          <span className="text-base font-semibold tracking-tight text-ink">OES</span>
          {collapsed ? null : <span className="type-meta">Console</span>}
        </Link>
        {onToggleCollapsed && !collapsed ? (
          <Button
            variant="ghost"
            size="icon"
            className="ml-auto hidden lg:inline-flex"
            aria-label="Collapse navigation"
            onClick={onToggleCollapsed}
          >
            <PanelLeftClose aria-hidden />
          </Button>
        ) : null}
      </div>

      {collapsed ? null : (
        <p className="mt-1 px-2 type-meta">
          {deployment.info.mode === 'cluster' ? 'Cluster' : 'Standalone'} ·{' '}
          {deployment.info.version}
        </p>
      )}

      {onToggleCollapsed && collapsed ? (
        <Button
          variant="ghost"
          size="icon"
          className="mt-2 hidden self-center lg:inline-flex"
          aria-label="Expand navigation"
          onClick={onToggleCollapsed}
        >
          <PanelLeftOpen aria-hidden />
        </Button>
      ) : null}

      <div className="mt-5 flex flex-1 flex-col gap-4">
        {sections.map((section) => (
          <div key={section.title} className="space-y-1">
            {collapsed ? (
              // A separator instead of a heading: a truncated word is worse
              // than none, and the grouping is still legible.
              <div className="mx-2 border-t border-border" aria-hidden />
            ) : (
              <p className="px-2 type-eyebrow">{section.title}</p>
            )}
            <ul className="space-y-0.5">
              {section.items.map((item) => {
                const active = isActive(item, pathname);
                const Icon = item.icon;
                return (
                  <li key={item.href}>
                    <Link
                      href={item.href}
                      onClick={onNavigate}
                      aria-current={active ? 'page' : undefined}
                      title={collapsed ? item.label : undefined}
                      className={cn(
                        'relative flex items-center gap-2.5 rounded-[--radius-control] text-sm',
                        collapsed ? 'justify-center px-2 py-2' : 'px-2 py-1.5',
                        active
                          ? 'bg-accent-soft font-medium text-accent'
                          : 'text-ink-muted hover:bg-surface-muted hover:text-ink',
                      )}
                    >
                      {/* An accent rail as well as the tint, so the active item
                          is not signalled by colour alone. */}
                      {active ? (
                        <span
                          aria-hidden
                          className="absolute inset-y-1 left-0 w-0.5 rounded-full bg-accent"
                        />
                      ) : null}
                      <Icon aria-hidden className="size-4 shrink-0" />
                      {collapsed ? (
                        <span className="sr-only">{item.label}</span>
                      ) : (
                        <span className="truncate">{item.label}</span>
                      )}
                    </Link>
                  </li>
                );
              })}
            </ul>
          </div>
        ))}
      </div>

      <div className={cn('mt-4 border-t border-border pt-3', collapsed ? '' : 'px-1')}>
        <AccountMenu deployment={deployment} collapsed={collapsed} />
      </div>
    </nav>
  );
}

function TopBar({
  onOpenNavigation,
  mobileOpen,
  onOpenPalette,
}: {
  readonly onOpenNavigation: () => void;
  readonly mobileOpen: boolean;
  readonly onOpenPalette: () => void;
}) {
  return (
    <header className="sticky top-0 z-30 flex h-14 items-center gap-3 border-b border-border bg-surface/90 px-4 backdrop-blur-sm sm:px-6">
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

      {/*
        Deliberately sparse. Page-specific actions belong in the page header, so
        the only things here are the two that apply everywhere: finding something
        and choosing a theme.
      */}
      <div className="min-w-0 flex-1">
        <CommandTrigger onOpen={onOpenPalette} />
      </div>

      <ThemeToggle />
    </header>
  );
}

/**
 * The signed-in role and the way out.
 *
 * At the foot of the sidebar rather than the top bar: it is the least-used
 * control in the console, and putting it beside the navigation keeps the top bar
 * for things that act on the current page.
 */
function AccountMenu({
  deployment,
  collapsed,
}: {
  readonly deployment: Deployment;
  readonly collapsed: boolean;
}) {
  const router = useRouter();
  const [signingOut, setSigningOut] = React.useState(false);
  const role = ROLE_LABEL[deployment.session.role] ?? deployment.session.role;

  async function signOut() {
    setSigningOut(true);
    try {
      await fetch('/api/auth/logout', { method: 'POST', credentials: 'same-origin' });
    } finally {
      router.replace('/login');
    }
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          size={collapsed ? 'icon' : 'sm'}
          className={collapsed ? 'w-full' : 'w-full justify-start gap-2'}
          aria-label={`Account: ${role}`}
        >
          <UserRound aria-hidden className="size-4 shrink-0" />
          {collapsed ? null : <span className="truncate text-sm">{role}</span>}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" side="top">
        <DropdownMenuLabel>Signed in</DropdownMenuLabel>
        <div className="px-2 pb-2">
          <Badge tone="accent">{role}</Badge>
        </div>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={signOut} disabled={signingOut}>
          <LogOut aria-hidden />
          {signingOut ? 'Signing out…' : 'Sign out'}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
