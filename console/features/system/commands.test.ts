import { describe, expect, it } from 'vitest';

import { buildCommands, matchCommands } from './commands';
import type { NavSection } from '@/features/system/navigation';
import type { RolePermissions } from '@/types/api';

const sections: readonly NavSection[] = [
  { title: 'Storage', items: [{ href: '/buckets', label: 'Buckets' }] },
  { title: 'Access', items: [{ href: '/service-accounts', label: 'Service accounts' }] },
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
