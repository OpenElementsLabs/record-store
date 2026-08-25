import { Circle } from 'lucide-react';
import { describe, expect, it } from 'vitest';

import { buildCommands, buildEntityCommands, matchCommands } from './commands';
import type { NavSection } from '@/features/system/navigation';
import type { RolePermissions } from '@/types/api';

const sections: readonly NavSection[] = [
  { title: 'Storage', items: [{ href: '/buckets', label: 'Buckets', icon: Circle }] },
  {
    title: 'Access',
    items: [{ href: '/service-accounts', label: 'Service accounts', icon: Circle }],
  },
];

const admin: RolePermissions = {
  manage_buckets: true,
  manage_objects: true,
  manage_service_accounts: true,
  manage_policies: true,
  manage_webhooks: true,
  read_audit: true,
  manage_cluster: true,
  manage_storage: true,
  manage_sharing: true,
};

const auditor: RolePermissions = {
  manage_buckets: false,
  manage_objects: false,
  manage_service_accounts: false,
  manage_policies: false,
  manage_webhooks: false,
  read_audit: true,
  manage_cluster: false,
  manage_storage: false,
  manage_sharing: false,
};

describe('buildCommands', () => {
  it('offers every screen the operator can already reach', () => {
    const commands = buildCommands(sections, admin);
    expect(commands.map((command) => command.label)).toContain('Buckets');
    expect(commands.map((command) => command.label)).toContain('Service accounts');
  });

  it('offers no action a role cannot perform', () => {
    // The palette must not become a way around role gating.
    const labels = buildCommands(sections, auditor).map((command) => command.label);
    expect(labels).not.toContain('Create bucket');
    expect(labels).not.toContain('Create service account');
    expect(labels).not.toContain('Create policy');
  });

  it('cannot offer a screen the navigation hides', () => {
    // Deriving from navigation is what guarantees this: an empty nav yields no
    // navigation commands at all.
    const commands = buildCommands([], auditor);
    expect(commands).toHaveLength(0);
  });
});

describe('matchCommands', () => {
  const commands = buildCommands(sections, admin);

  it('returns everything for an empty query', () => {
    expect(matchCommands(commands, '  ')).toHaveLength(commands.length);
  });

  it('matches on a subsequence the way a palette should', () => {
    const found = matchCommands(commands, 'sacc').map((command) => command.label);
    expect(found).toContain('Service accounts');
  });

  it('matches the group as well as the label', () => {
    const found = matchCommands(commands, 'storage').map((command) => command.label);
    expect(found).toContain('Buckets');
  });

  it('returns nothing when nothing matches', () => {
    expect(matchCommands(commands, 'zzzz')).toHaveLength(0);
  });
});

describe('buildEntityCommands', () => {
  const entities = {
    buckets: [{ id: 'b1', name: 'uploads' }],
    serviceAccounts: [{ id: 'a1', name: 'ingest' }],
    policies: [{ id: 'p1', name: 'readers' }],
    nodes: [{ id: 'n-1234567890', label: 'n-123456' }],
  };

  it('routes each entity to where it can be inspected', () => {
    const commands = buildEntityCommands(entities);
    expect(commands.find((c) => c.label === 'uploads')?.href).toBe('/buckets/uploads');
    expect(commands.find((c) => c.label === 'ingest')?.href).toBe('/service-accounts/a1');
    expect(commands.find((c) => c.label === 'n-123456')?.href).toBe('/cluster/nodes/n-1234567890');
  });

  it('groups entities by kind so the list stays readable', () => {
    const groups = new Set(buildEntityCommands(entities).map((c) => c.group));
    expect(groups).toEqual(new Set(['Buckets', 'Service accounts', 'Policies', 'Nodes']));
  });

  it('offers nothing for lists that were not loaded', () => {
    expect(
      buildEntityCommands({ buckets: [], serviceAccounts: [], policies: [], nodes: [] }),
    ).toHaveLength(0);
  });

  it('encodes names that are not URL-safe', () => {
    const commands = buildEntityCommands({
      ...entities,
      buckets: [{ id: 'b2', name: 'my bucket' }],
    });
    expect(commands.find((c) => c.label === 'my bucket')?.href).toBe('/buckets/my%20bucket');
  });

  it('never offers an object key, because that set is unbounded', () => {
    // Object search is server-side by prefix inside a bucket; the palette must
    // not imply it has loaded a bucket's contents.
    const groups = buildEntityCommands(entities).map((c) => c.group);
    expect(groups).not.toContain('Objects');
  });
});
