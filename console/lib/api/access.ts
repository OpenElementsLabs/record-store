import type { IssuedCredential, Policy, PolicyStatement, ServiceAccountInfo } from '@/types/api';

import { request, requestVoid } from './client';

export function fetchServiceAccounts(signal?: AbortSignal): Promise<ServiceAccountInfo[]> {
  return request<ServiceAccountInfo[]>('/v1/service-accounts', signal ? { signal } : {});
}

export function fetchServiceAccount(id: string, signal?: AbortSignal): Promise<ServiceAccountInfo> {
  return request<ServiceAccountInfo>(
    `/v1/service-accounts/${encodeURIComponent(id)}`,
    signal ? { signal } : {},
  );
}

/** Creates an account. The returned secret is shown exactly once. */
export function createServiceAccount(input: {
  name: string;
  description?: string;
}): Promise<IssuedCredential> {
  return request<IssuedCredential>('/v1/service-accounts', {
    method: 'POST',
    body: { name: input.name, description: input.description ?? '' },
  });
}

export function setServiceAccountEnabled(id: string, enabled: boolean): Promise<unknown> {
  return request<unknown>(`/v1/service-accounts/${encodeURIComponent(id)}/status`, {
    method: 'PUT',
    body: { enabled },
  });
}

export function deleteServiceAccount(id: string): Promise<void> {
  return requestVoid(`/v1/service-accounts/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

/**
 * Issues an additional credential for an account.
 *
 * Rotation deliberately leaves existing credentials active so an operator can
 * roll applications forward before revoking the old one.
 */
export function rotateCredential(id: string): Promise<IssuedCredential> {
  return request<IssuedCredential>(`/v1/service-accounts/${encodeURIComponent(id)}/credentials`, {
    method: 'POST',
    body: {},
  });
}

/**
 * Issues a credential that expires on its own.
 *
 * A temporary credential inherits the account's policies, so it grants no more
 * than the account already has. The backend bounds the lifetime between one
 * minute and one day; the secret is returned once and never again.
 */
export function issueTemporaryCredential(
  id: string,
  expiresInSeconds: number,
): Promise<IssuedCredential> {
  return request<IssuedCredential>(
    `/v1/service-accounts/${encodeURIComponent(id)}/temporary-credentials`,
    { method: 'POST', body: { expires_in_seconds: expiresInSeconds } },
  );
}

export function setCredentialEnabled(
  accountId: string,
  credentialId: string,
  enabled: boolean,
): Promise<unknown> {
  return request<unknown>(
    `/v1/service-accounts/${encodeURIComponent(accountId)}/credentials/${encodeURIComponent(
      credentialId,
    )}/status`,
    { method: 'PUT', body: { enabled } },
  );
}

export function fetchPolicies(signal?: AbortSignal): Promise<Policy[]> {
  return request<Policy[]>('/v1/policies', signal ? { signal } : {});
}

export function createPolicy(input: {
  name: string;
  description: string;
  statements: readonly PolicyStatement[];
}): Promise<Policy> {
  return request<Policy>('/v1/policies', { method: 'POST', body: input });
}

export function attachPolicy(policyId: string, accountId: string): Promise<unknown> {
  return request<unknown>(
    `/v1/policies/${encodeURIComponent(policyId)}/bindings/${encodeURIComponent(accountId)}`,
    { method: 'PUT', body: {} },
  );
}

export function detachPolicy(policyId: string, accountId: string): Promise<void> {
  return requestVoid(
    `/v1/policies/${encodeURIComponent(policyId)}/bindings/${encodeURIComponent(accountId)}`,
    { method: 'DELETE' },
  );
}
