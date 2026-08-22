import type { Capabilities, RolePermissions } from '@/types/api';

/** One navigable screen. */
export type NavItem = {
  readonly href: string;
  readonly label: string;
  /** Matches nested routes, so a child page keeps its parent highlighted. */
  readonly prefix?: string;
};

export type NavSection = {
  readonly title: string;
  readonly items: readonly NavItem[];
};

export type NavigationInput = {
  readonly clusterEnabled: boolean;
  readonly capabilities: Capabilities;
  readonly permissions: RolePermissions;
};

/**
 * Builds the navigation for the current deployment and role.
 *
 * Sections are omitted rather than disabled: a standalone operator never sees
 * cluster concepts, and a role never sees a screen it cannot open. Empty
 * sections are dropped entirely so the sidebar only ever lists working screens.
 */
export function buildNavigation(input: NavigationInput): readonly NavSection[] {
  const { clusterEnabled, capabilities, permissions } = input;

  const sections: NavSection[] = [
    { title: 'Overview', items: [{ href: '/', label: 'Overview' }] },
    {
      title: 'Storage',
      items: [{ href: '/buckets', label: 'Buckets', prefix: '/buckets' }],
    },
  ];

  // Integrity reads the storage catalog and can reclaim orphaned payloads, so
  // it belongs to the storage-administration role rather than to everyone.
  const data: NavItem[] = [];
  if (permissions.manage_storage) {
    data.push({ href: '/integrity', label: 'Integrity', prefix: '/integrity' });
  }
  if (data.length > 0) sections.push({ title: 'Data management', items: data });

  const access: NavItem[] = [];
  if (permissions.manage_service_accounts) {
    access.push({
      href: '/service-accounts',
      label: 'Service accounts',
      prefix: '/service-accounts',
    });
  }
  if (permissions.manage_policies) {
    access.push({ href: '/policies', label: 'Policies', prefix: '/policies' });
  }
  if (access.length > 0) sections.push({ title: 'Access', items: access });

  const operations: NavItem[] = [];
  if (capabilities.events) {
    operations.push({ href: '/events', label: 'Events', prefix: '/events' });
  }
  if (capabilities.webhooks && permissions.manage_webhooks) {
    operations.push({ href: '/webhooks', label: 'Webhooks', prefix: '/webhooks' });
  }
  if (permissions.read_audit) {
    operations.push({ href: '/audit', label: 'Audit log', prefix: '/audit' });
  }
  if (operations.length > 0) sections.push({ title: 'Operations', items: operations });

  if (clusterEnabled) {
    sections.push({
      title: 'Cluster',
      items: [
        { href: '/cluster', label: 'Cluster overview' },
        { href: '/cluster/nodes', label: 'Nodes', prefix: '/cluster/nodes' },
      ],
    });
  }

  const system: NavItem[] = [{ href: '/system', label: 'Health' }];
  // Metrics read the same counters Prometheus scrapes, through the management
  // plane, so they are available to every role that can reach the console.
  system.push({ href: '/metrics', label: 'Metrics', prefix: '/metrics' });
  sections.push({ title: 'System', items: system });
  return sections;
}

/** Whether a nav item matches the current path. */
export function isActive(item: NavItem, pathname: string): boolean {
  if (item.prefix) return pathname === item.href || pathname.startsWith(`${item.prefix}/`);
  return pathname === item.href;
}
