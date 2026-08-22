import { describe, expect, it } from 'vitest';

import type { Capabilities, RolePermissions } from '@/types/api';

import { buildNavigation, isActive } from './navigation';

const allCapabilities: Capabilities = {
  cluster: true,
  versioning: true,
  webhooks: true,
  events: true,
  lifecycle: true,
  object_browser: true,
  erasure_coding: false,
};

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

function titles(sections: readonly { title: string }[]): string[] {
  return sections.map((section) => section.title);
}

function hrefs(sections: readonly { items: readonly { href: string }[] }[]): string[] {
  return sections.flatMap((section) => section.items.map((item) => item.href));
}

describe('buildNavigation', () => {
  it('hides every cluster concept in standalone mode', () => {
    const sections = buildNavigation({
      clusterEnabled: false,
      capabilities: { ...allCapabilities, cluster: false },
      permissions: admin,
    });
    expect(titles(sections)).not.toContain('Cluster');
    expect(hrefs(sections)).not.toContain('/cluster');
    expect(hrefs(sections)).not.toContain('/cluster/nodes');
    // A standalone operator still gets the full storage experience.
    expect(hrefs(sections)).toContain('/buckets');
    expect(hrefs(sections)).toContain('/system');
  });

  it('exposes cluster screens once the backend reports cluster mode', () => {
    const sections = buildNavigation({
      clusterEnabled: true,
      capabilities: allCapabilities,
      permissions: admin,
    });
    expect(titles(sections)).toContain('Cluster');
    expect(hrefs(sections)).toContain('/cluster/nodes');
  });

  it('omits screens the role cannot open rather than disabling them', () => {
    const sections = buildNavigation({
      clusterEnabled: false,
      capabilities: allCapabilities,
      permissions: auditor,
    });
    const links = hrefs(sections);
    expect(links).not.toContain('/service-accounts');
    expect(links).not.toContain('/policies');
    expect(links).not.toContain('/webhooks');
    // An auditor keeps the read-only screens their role is for.
    expect(links).toContain('/audit');
    expect(links).toContain('/events');
    expect(titles(sections)).not.toContain('Access');
  });

  it('drops a section entirely when it would be empty', () => {
    const sections = buildNavigation({
      clusterEnabled: false,
      capabilities: { ...allCapabilities, events: false, webhooks: false },
      permissions: { ...admin, read_audit: false },
    });
    expect(titles(sections)).not.toContain('Operations');
  });

  it('hides features the deployment does not report as available', () => {
    const sections = buildNavigation({
      clusterEnabled: false,
      capabilities: { ...allCapabilities, webhooks: false },
      permissions: admin,
    });
    expect(hrefs(sections)).not.toContain('/webhooks');
    expect(hrefs(sections)).toContain('/events');
  });
});

describe('isActive', () => {
  it('matches exact routes', () => {
    expect(isActive({ href: '/', label: 'Overview' }, '/')).toBe(true);
    expect(isActive({ href: '/', label: 'Overview' }, '/buckets')).toBe(false);
  });

  it('keeps a parent highlighted on nested routes', () => {
    const item = { href: '/buckets', label: 'Buckets', prefix: '/buckets' };
    expect(isActive(item, '/buckets')).toBe(true);
    expect(isActive(item, '/buckets/uploads')).toBe(true);
    expect(isActive(item, '/bucketsomething')).toBe(false);
  });
});
