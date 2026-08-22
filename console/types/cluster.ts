/**
 * Types for the cluster surface of the management API.
 *
 * These are only meaningful when the backend reports `mode: "cluster"`. A
 * standalone deployment never exposes them, and the console never renders them.
 */

export type ClusterHealthLevel = 'healthy' | 'degraded' | 'critical' | 'unavailable';

export type NodeState =
  | 'joining'
  | 'healthy'
  | 'suspect'
  | 'unreachable'
  | 'draining'
  | 'maintenance'
  | 'offline'
  | 'decommissioned';

export type QuorumStatus = {
  readonly members: number;
  readonly healthy_members: number;
  readonly quorum: number;
  readonly leader: string | null;
  readonly writable: boolean;
  readonly readable: boolean;
  readonly health: ClusterHealthLevel;
  /** Whether the group has enough voters to survive losing one member. */
  readonly fault_tolerant: boolean;
  readonly notes: readonly string[];
};

export type ConsensusMember = {
  readonly member_id: number;
  readonly address: string;
  readonly voter: boolean;
  readonly reachable: boolean;
};

export type MetadataQuorum = {
  readonly status: QuorumStatus;
  readonly role: string;
  readonly member_id: number;
  readonly applied_index: number | null;
  readonly last_log_index: number | null;
  readonly snapshot_index: number | null;
  readonly members: readonly ConsensusMember[];
};

export type DataHealth = {
  readonly nodes: number;
  readonly healthy_nodes: number;
  readonly unavailable_nodes: number;
  readonly under_replicated_payloads: number;
  readonly unavailable_payloads: number;
  readonly writable: boolean;
  readonly health: ClusterHealthLevel;
  readonly notes: readonly string[];
};

export type ClusterNode = {
  readonly node_id: string;
  readonly member_id: number;
  readonly state: NodeState;
  readonly metadata_voter: boolean;
  readonly rpc_address: string;
  readonly storage_class: string;
  readonly failure_domain: Readonly<Record<string, string>>;
  readonly software_version: string;
  readonly capacity_bytes: number;
  readonly available_bytes: number;
  readonly utilization_percent: number;
  readonly replicas: number;
  readonly last_heartbeat_at: string | null;
  readonly state_changed_at: string;
  readonly state_reason: string | null;
};

export type ReplicationStatus = {
  readonly replication_factor: number;
  readonly required_acknowledgements: number;
  readonly payloads: number;
  readonly logical_bytes: number;
  readonly physical_bytes: number;
  readonly under_replicated_payloads: number;
  readonly unavailable_payloads: number;
  readonly tombstones: number;
};

export type RepairStatus = {
  readonly active_tasks: number;
  readonly parked_tasks: number;
};

export type ClusterOperationKind = 'drain' | 'rebalance' | 'decommission';

export type ClusterOperationState =
  'planning' | 'moving' | 'verifying' | 'completed' | 'cancelled' | 'failed';

export type OperationProgress = {
  readonly objects_remaining: number;
  readonly bytes_remaining: number;
  readonly objects_moved: number;
  readonly bytes_moved: number;
  readonly replicas_moving: number;
  readonly tasks_parked: number;
};

export type ClusterOperation = {
  readonly id: string;
  readonly kind: ClusterOperationKind;
  readonly node_id: string | null;
  readonly state: ClusterOperationState;
  readonly progress: OperationProgress;
  readonly started_at: string;
  readonly updated_at: string;
  readonly completed_at: string | null;
  readonly message: string | null;
};

export type BackgroundTaskStatus =
  | { readonly state: 'running'; readonly last_pass_at: string | null }
  | { readonly state: 'stopped' }
  | { readonly state: 'failed'; readonly reason: string; readonly at: string };

export type ClusterStatus = {
  readonly cluster_id: string;
  readonly health: ClusterHealthLevel;
  readonly metadata: MetadataQuorum;
  readonly data: DataHealth;
  readonly replication: ReplicationStatus;
  readonly repair: RepairStatus;
  readonly nodes: readonly ClusterNode[];
  readonly operations: readonly ClusterOperation[];
  readonly local_tasks: Readonly<Record<string, BackgroundTaskStatus>>;
  readonly observed_at: string;
};

export type ClusterHealthReport = {
  readonly health: ClusterHealthLevel;
  readonly reasons: readonly string[];
  readonly metadata: MetadataQuorum;
  readonly data: DataHealth;
};

/** Why a node cannot be removed without losing durability. */
export type DecommissionSafety = {
  readonly node_id: string;
  readonly safe: boolean;
  readonly at_risk_payloads: number;
  readonly unavailable_payloads: number;
  readonly replicas_remaining: number;
  readonly bytes_remaining: number;
  readonly reason: string;
};
