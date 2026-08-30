import type { LucideIcon } from 'lucide-react';
import {
  Activity,
  Boxes,
  Database,
  FileStack,
  Gauge,
  HardDrive,
  HeartPulse,
  KeyRound,
  LayoutDashboard,
  Network,
  ScrollText,
  Scale,
  ShieldCheck,
  Server,
  Shuffle,
  Webhook,
} from 'lucide-react';

import type { Capabilities, RolePermissions } from '@/types/api';

/** One navigable screen. */
export type NavItem = {
  readonly href: string;
  readonly label: string;
  /** Matches nested routes, so a child page keeps its parent highlighted. */
  readonly prefix?: string;
  /**
   * The item's icon.
   *
   * Required rather than optional: a collapsed sidebar shows nothing but icons,
   * so an item without one would simply disappear.
   */
  readonly icon: LucideIcon;
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
    { title: 'Overview', items: [{ href: '/', label: 'Overview', icon: LayoutDashboard }] },
    {
      title: 'Storage',
      items: [{ href: '/buckets', label: 'Buckets', prefix: '/buckets', icon: Database }],
    },
  ];

  // Integrity reads the storage catalog and can reclaim orphaned payloads, so
  // it belongs to the storage-administration role rather than to everyone.
  const data: NavItem[] = [];
  if (permissions.manage_storage) {
    data.push({ href: '/integrity', label: 'Integrity', prefix: '/integrity', icon: ShieldCheck });
  }
  if (data.length > 0) sections.push({ title: 'Data management', items: data });

  const access: NavItem[] = [];
  if (permissions.manage_service_accounts) {
    access.push({
      href: '/service-accounts',
      label: 'Service accounts',
      prefix: '/service-accounts',
      icon: KeyRound,
    });
  }
  if (permissions.manage_policies) {
    access.push({ href: '/policies', label: 'Policies', prefix: '/policies', icon: Scale });
  }
  if (access.length > 0) sections.push({ title: 'Access', items: access });

  const operations: NavItem[] = [];
  if (capabilities.events) {
    operations.push({ href: '/events', label: 'Events', prefix: '/events', icon: Activity });
  }
  if (capabilities.webhooks && permissions.manage_webhooks) {
    operations.push({ href: '/webhooks', label: 'Webhooks', prefix: '/webhooks', icon: Webhook });
  }
  if (permissions.read_audit) {
    operations.push({ href: '/audit', label: 'Audit log', prefix: '/audit', icon: ScrollText });
  }
  if (operations.length > 0) sections.push({ title: 'Operations', items: operations });

  if (clusterEnabled) {
    const cluster: NavItem[] = [
      { href: '/cluster', label: 'Cluster overview', icon: Boxes },
      { href: '/cluster/nodes', label: 'Nodes', prefix: '/cluster/nodes', icon: Server },
      { href: '/cluster/drives', label: 'Drives', prefix: '/cluster/drives', icon: HardDrive },
      {
        href: '/cluster/durability',
        label: 'Durability',
        prefix: '/cluster/durability',
        icon: FileStack,
      },
      {
        href: '/cluster/consensus',
        label: 'Consensus',
        prefix: '/cluster/consensus',
        icon: Network,
      },
    ];
    // Rebalancing moves data between nodes, so it is offered only to a role
    // that may operate the cluster.
    if (permissions.manage_cluster) {
      cluster.push({
        href: '/cluster/rebalance',
        label: 'Rebalancing',
        prefix: '/cluster/rebalance',
        icon: Shuffle,
      });
    }
    sections.push({ title: 'Cluster', items: cluster });
  }

  const system: NavItem[] = [{ href: '/system', label: 'Health', icon: HeartPulse }];
  // Metrics read the same counters Prometheus scrapes, through the management
  // plane, so they are available to every role that can reach the console.
  system.push({ href: '/metrics', label: 'Metrics', prefix: '/metrics', icon: Gauge });
  sections.push({ title: 'System', items: system });
  return sections;
}

/** Whether a nav item matches the current path. */
export function isActive(item: NavItem, pathname: string): boolean {
  if (item.prefix) return pathname === item.href || pathname.startsWith(`${item.prefix}/`);
  return pathname === item.href;
}
