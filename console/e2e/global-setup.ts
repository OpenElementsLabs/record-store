import type { FullConfig } from '@playwright/test';

/** Refuses to run UI tests until the owned backend proves its OES identity. */
export default async function globalSetup(_config: FullConfig): Promise<void> {
  const managementUrl = required('OES_E2E_MANAGEMENT_URL');
  const consoleUrl = required('OES_E2E_CONSOLE_URL');
  const expectedMode = required('OES_E2E_EXPECTED_MODE');
  const token = required('OES_E2E_TOKEN');

  const response = await fetch(`${managementUrl}/api/v1/system/info`, {
    headers: { authorization: `Bearer ${token}` },
  });
  if (!response.ok) {
    throw new Error(`E2E backend identity check failed with HTTP ${response.status}`);
  }
  const identity = (await response.json()) as Record<string, unknown>;
  if (
    identity.name !== 'oes' ||
    identity.mode !== expectedMode ||
    typeof identity.version !== 'string'
  ) {
    throw new Error(`E2E backend identity mismatch: ${JSON.stringify(identity)}`);
  }

  const consoleResponse = await fetch(consoleUrl, { redirect: 'manual' });
  if (!consoleResponse.ok && ![301, 302, 303, 307, 308].includes(consoleResponse.status)) {
    throw new Error(`E2E console readiness check failed with HTTP ${consoleResponse.status}`);
  }
}

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required by E2E global setup`);
  return value;
}
