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
  /** Absent when this member cannot observe peer reachability — only the leader can. */
  readonly healthy_members: number | null;
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
  /** `null` when this member cannot observe it; only the leader tracks replication contact. */
  readonly reachable: boolean | null;
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

/** Physical technology a device reports, or `unknown` when the platform did not say. */
export type DeviceKind =
  | 'nvme'
  | 'sata_ssd'
  | 'sas_ssd'
  | 'sata_hdd'
  | 'sas_hdd'
  | 'ssd'
  | 'hdd'
  | 'block_device'
  | 'raid_logical_volume'
  | 'cloud_block_volume'
  | 'filesystem_directory'
  | 'unknown';

/** Durable administrative lifecycle of a registered device. */
export type DeviceState =
  | 'discovered'
  | 'available'
  | 'active'
  | 'degraded'
  | 'draining'
  | 'maintenance'
  | 'failed'
  | 'safe_to_remove'
  | 'retired';

/**
 * Best available health observation.
 *
 * `unknown` and `unsupported` are real values, not placeholders: the platform
 * either did not report health or cannot. Neither is inferred from lifecycle.
 */
export type DeviceHealth =
  'unknown' | 'healthy' | 'degraded' | 'failed' | 'unavailable' | 'unsupported';

export type ClusterDevice = {
  readonly device_id: string;
  readonly node_id: string;
  readonly kind: DeviceKind;
  readonly storage_class: string;
  readonly state: DeviceState;
  readonly health: DeviceHealth;
  readonly capacity_bytes: number;
  readonly usable_bytes: number;
  readonly available_bytes: number;
  readonly utilization_percent: number;
  readonly configured_weight: number;
  readonly accepts_placement: boolean;
  readonly current_path: string | null;
  readonly model: string | null;
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
  readonly devices?: readonly ClusterDevice[];
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
