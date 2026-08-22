import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { render, type RenderResult } from '@testing-library/react';
import type * as React from 'react';

import { DeploymentProvider } from '@/features/system/deployment';
import type { Capabilities, RolePermissions, Session, SystemInfo } from '@/types/api';

export const allCapabilities: Capabilities = {
  cluster: false,
  versioning: true,
  webhooks: true,
  events: true,
  lifecycle: true,
  object_browser: true,
  erasure_coding: false,
};

export const adminPermissions: RolePermissions = {
  manage_buckets: true,
  manage_objects: true,
  manage_service_accounts: true,
  manage_policies: true,
  manage_webhooks: true,
  read_audit: true,
  manage_cluster: true,
  manage_storage: true,
};

export const auditorPermissions: RolePermissions = {
  manage_buckets: false,
  manage_objects: false,
  manage_service_accounts: false,
  manage_policies: false,
  manage_webhooks: false,
  read_audit: true,
  manage_cluster: false,
  manage_storage: false,
};

export function systemInfo(overrides: Partial<SystemInfo> = {}): SystemInfo {
  return {
    name: 'oes',
    version: '0.1.0',
    status: 'ready',
    mode: 'standalone',
    capabilities: allCapabilities,
    ...overrides,
  };
}

export function session(permissions: RolePermissions = adminPermissions): Session {
  return { role: 'system_administrator', permissions };
}

/**
 * Renders a component with the providers the console supplies at runtime.
 *
 * Retries are disabled so a test asserting a failure state does not wait for
 * the production retry policy.
 */
export function renderWithProviders(
  ui: React.ReactNode,
  options: { info?: SystemInfo; session?: Session } = {},
): RenderResult & { client: QueryClient } {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 }, mutations: { retry: false } },
  });
  const result = render(
    <QueryClientProvider client={client}>
      <DeploymentProvider
        value={{ info: options.info ?? systemInfo(), session: options.session ?? session() }}
      >
        {ui}
      </DeploymentProvider>
    </QueryClientProvider>,
  );
  return { ...result, client };
}

/** Builds a management API error envelope with the documented shape. */
export function errorBody(code: string, message: string, requestId = 'req-test') {
  return { error: { code, message, request_id: requestId } };
}

/** A `fetch` stub that answers a single JSON response. */
export function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}
