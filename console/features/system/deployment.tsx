'use client';

import * as React from 'react';

import type { Capabilities, RolePermissions, Session, SystemInfo } from '@/types/api';

/**
 * What the console knows about the deployment it is attached to.
 *
 * Deployment mode and capabilities come from the backend, never from the
 * console's own environment, so one build serves standalone and cluster
 * installations without a rebuild.
 */
export type Deployment = {
  readonly info: SystemInfo;
  readonly session: Session;
};

const DeploymentContext = React.createContext<Deployment | null>(null);

export function DeploymentProvider({
  value,
  children,
}: {
  readonly value: Deployment;
  readonly children: React.ReactNode;
}) {
  return <DeploymentContext.Provider value={value}>{children}</DeploymentContext.Provider>;
}

/** Reads the deployment. Only valid inside the authenticated shell. */
export function useDeployment(): Deployment {
  const value = React.useContext(DeploymentContext);
  if (!value) {
    throw new Error('useDeployment must be used inside DeploymentProvider');
  }
  return value;
}

/**
 * Reads the capability set.
 *
 * Components ask about a capability rather than checking the deployment mode, so
 * a build that gains or loses a feature does not require touching every screen.
 */
export function useCapabilities(): Capabilities {
  return useDeployment().info.capabilities;
}

/**
 * Reads the current role's permissions.
 *
 * These decide what the console offers. They are a usability aid: the backend
 * enforces every permission independently, so hiding a control is never the
 * security boundary.
 */
export function usePermissions(): RolePermissions {
  return useDeployment().session.permissions;
}

/**
 * Whether cluster screens should exist at all in this deployment.
 *
 * This asks the capability, not the mode. A `control` process is a cluster
 * member that serves the management API and holds no replicas, so it is exactly
 * what a console is normally pointed at; gating on `mode === 'cluster'` hid every
 * cluster screen from the one node type most likely to be serving them.
 */
export function useClusterEnabled(): boolean {
  return useDeployment().info.capabilities.cluster;
}

/** Human label for the deployment mode. */
export function deploymentModeLabel(mode: SystemInfo['mode']): string {
  switch (mode) {
    case 'cluster':
      return 'Cluster';
    case 'control':
      return 'Control';
    default:
      return 'Standalone';
  }
}
