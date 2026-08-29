import { renderHook } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import {
  DeploymentProvider,
  deploymentModeLabel,
  useClusterEnabled,
} from '@/features/system/deployment';
import { session, systemInfo } from '@/test/render';

import type { DeploymentMode, SystemInfo } from '@/types/api';

/** Builds system info with both dimensions stated, since they vary independently. */
function info(mode: DeploymentMode, cluster: boolean): SystemInfo {
  const base = systemInfo();
  return { ...base, mode, capabilities: { ...base.capabilities, cluster } };
}

function clusterEnabledFor(value: SystemInfo): boolean {
  const { result } = renderHook(() => useClusterEnabled(), {
    wrapper: ({ children }) => (
      <DeploymentProvider value={{ info: value, session: session() }}>
        {children}
      </DeploymentProvider>
    ),
  });
  return result.current;
}

describe('useClusterEnabled', () => {
  it('is false for a standalone deployment', () => {
    expect(clusterEnabledFor(info('standalone', false))).toBe(false);
  });

  it('is true for a storage node in a cluster', () => {
    expect(clusterEnabledFor(info('cluster', true))).toBe(true);
  });

  /**
   * A control process is a cluster member that serves the management API and
   * holds no replicas, so it is what a console is normally pointed at — the
   * shipped cluster Compose file does exactly that. Gating on `mode === 'cluster'`
   * hid every cluster screen from that node, so a healthy cluster read as
   * standalone and Consensus and Replication showed as "Not enabled".
   */
  it('is true for a control node, which is a cluster member serving the management API', () => {
    expect(clusterEnabledFor(info('control', true))).toBe(true);
  });

  /**
   * The capability is the backend's own answer to "is this part of a cluster",
   * so it decides — the mode is a label, not the gate.
   */
  it('follows the capability rather than the mode', () => {
    expect(clusterEnabledFor(info('cluster', false))).toBe(false);
  });
});

describe('deploymentModeLabel', () => {
  it('names every mode distinctly, so a control node is not reported as standalone', () => {
    expect(deploymentModeLabel('standalone')).toBe('Standalone');
    expect(deploymentModeLabel('cluster')).toBe('Cluster');
    expect(deploymentModeLabel('control')).toBe('Control');
  });
});
