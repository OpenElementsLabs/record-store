/**
 * Types for the OES management API on port 7601.
 *
 * These mirror the Rust API's serialized shapes. They are written by hand
 * because the surface the console uses is small and reviewable; a generation
 * pipeline would add more moving parts than it removes.
 */

export type DeploymentMode = 'standalone' | 'cluster' | 'control';

/** What a deployment can actually do, as reported by the backend. */
export type Capabilities = {
  readonly cluster: boolean;
  readonly versioning: boolean;
  readonly webhooks: boolean;
  readonly events: boolean;
  readonly lifecycle: boolean;
  readonly object_browser: boolean;
  readonly erasure_coding: boolean;
};

export type SystemInfo = {
  readonly name: string;
  readonly version: string;
  readonly status: string;
  readonly mode: DeploymentMode;
  readonly cluster_id?: string;
  readonly capabilities: Capabilities;
};

export type ManagementRole = 'system_administrator' | 'storage_administrator' | 'auditor';

/**
 * Coarse permissions a role grants.
 *
 * These drive what the console offers. They are a usability aid only: the API
 * enforces every permission independently.
 */
export type RolePermissions = {
  readonly manage_buckets: boolean;
  readonly manage_objects: boolean;
  readonly manage_service_accounts: boolean;
  readonly manage_policies: boolean;
  readonly manage_webhooks: boolean;
  readonly read_audit: boolean;
  readonly manage_cluster: boolean;
  readonly manage_storage: boolean;
};

export type Session = {
  readonly role: ManagementRole;
  readonly permissions: RolePermissions;
};

export type VersioningState = 'disabled' | 'enabled' | 'suspended';

export type ByteQuota =
  { readonly mode: 'unlimited' } | { readonly mode: 'limit'; readonly bytes: number };

export type ObjectCountQuota =
  { readonly mode: 'unlimited' } | { readonly mode: 'limit'; readonly objects: number };

export type BucketQuota = {
  readonly bytes: ByteQuota;
  readonly objects: ObjectCountQuota;
};

export type Bucket = {
  readonly id: string;
  readonly organization_id: string;
  readonly name: string;
  readonly created_at: string;
  readonly versioning: VersioningState;
  readonly quota: BucketQuota;
  readonly object_count: number;
  readonly logical_bytes: number;
  readonly version_count: number;
  readonly version_bytes: number;
  readonly multipart_bytes: number;
};

export type StorageUsage = {
  readonly object_count: number;
  readonly bytes_used: number;
  readonly bucket_count: number;
  readonly version_count: number;
  readonly version_bytes: number;
  readonly physical_bytes: number;
  readonly temporary_multipart_bytes: number;
};

export type StorageStatus = {
  readonly capacity_bytes: number;
  readonly available_bytes: number;
  readonly temporary_upload_bytes: number;
};

export type ObjectSummary = {
  readonly key: string;
  readonly size: number;
  readonly content_type: string | null;
  readonly etag: string;
  readonly checksum: string;
  readonly version_id: string;
  readonly created_at: string;
  readonly modified_at: string;
  readonly custom_metadata: Readonly<Record<string, string>>;
};

export type ObjectListPage = {
  readonly objects: readonly ObjectSummary[];
  /** Logical prefixes produced by the delimiter. OES stores no directories. */
  readonly prefixes: readonly string[];
  readonly is_truncated: boolean;
  readonly next_continuation_token: string | null;
};

export type ObjectVersionEntry = {
  readonly key: string;
  readonly version_id: string;
  readonly is_latest: boolean;
  readonly is_delete_marker: boolean;
  readonly is_null: boolean;
  readonly created_at: string;
  readonly size: number | null;
  readonly etag: string | null;
  readonly checksum: string | null;
};

export type ObjectVersionPage = {
  readonly versions: readonly ObjectVersionEntry[];
  readonly next_key_marker: string | null;
  readonly next_version_id_marker: string | null;
};

export type ServiceAccount = {
  readonly id: string;
  readonly organization_id: string;
  readonly name: string;
  readonly description: string;
  readonly disabled: boolean;
  readonly created_at: string;
  readonly updated_at: string;
};

export type Credential = {
  readonly id: string;
  readonly service_account_id: string;
  readonly key_id: string;
  readonly disabled: boolean;
  readonly created_at: string;
  readonly expires_at: string | null;
};

export type ServiceAccountInfo = {
  readonly account: ServiceAccount;
  readonly credential: Credential;
  readonly credentials: readonly Credential[];
  readonly policy_bindings: readonly string[];
};

/** A newly issued credential. The secret is returned exactly once. */
export type IssuedCredential = {
  readonly account: ServiceAccount;
  readonly credential: Credential;
  readonly secret_access_key: string;
};

export type PolicyEffect = 'allow' | 'deny';

export type PolicyAction =
  | 's3:ListBucket'
  | 's3:GetObject'
  | 's3:PutObject'
  | 's3:DeleteObject'
  | 's3:GetObjectVersion'
  | 's3:DeleteObjectVersion'
  | 's3:ManageBucket';

export type PolicyStatement = {
  readonly effect: PolicyEffect;
  readonly actions: readonly PolicyAction[];
  readonly resources: readonly string[];
};

export type Policy = {
  readonly id: string;
  readonly name: string;
  readonly description: string;
  readonly statements: readonly PolicyStatement[];
  readonly created_at: string;
  readonly updated_at: string;
};

export type AuditResult = 'success' | 'denied' | 'failure';

export type AuditEvent = {
  readonly event_id: string;
  readonly timestamp: string;
  readonly request_id: string | null;
  readonly principal: string;
  readonly credential_id: string | null;
  readonly source_ip: string | null;
  readonly operation: string;
  readonly resource: string;
  readonly result: AuditResult;
  readonly metadata: Readonly<Record<string, string>>;
};

export type AuditPage = {
  readonly events: readonly AuditEvent[];
  readonly next_time: string | null;
  readonly next_id: string | null;
};

export type StorageEventType =
  | 'bucket.created'
  | 'bucket.deleted'
  | 'object.created'
  | 'object.updated'
  | 'object.deleted'
  | 'object.restored'
  | 'multipart.completed'
  | 'multipart.aborted';

export type StorageEvent = {
  readonly id: string;
  readonly type: StorageEventType;
  readonly time: string;
  readonly bucket: string;
  readonly object: string | null;
  readonly version_id: string | null;
  readonly size: number | null;
  readonly metadata: Readonly<Record<string, string>>;
};

export type StorageEventPage = {
  readonly events: readonly StorageEvent[];
  readonly next_time: string | null;
  readonly next_id: string | null;
};

export type WebhookSubscription = {
  readonly id: string;
  readonly target_url: string;
  readonly event_types: readonly StorageEventType[];
  readonly bucket_filter: string | null;
  readonly object_prefix_filter: string | null;
  readonly enabled: boolean;
  readonly created_at: string;
};

/** A newly created webhook. The signing secret is returned exactly once. */
export type CreatedWebhook = {
  readonly subscription: WebhookSubscription;
  readonly signing_secret: string;
};

export type WebhookDeliveryLog = {
  readonly webhook_id: string;
  readonly event_id: string;
  readonly attempts: number;
  readonly success: boolean;
  readonly status_code: number | null;
  readonly error: string | null;
  readonly delivered_at: string;
};

export type LifecycleRule = {
  readonly id: string;
  readonly bucket_id: string;
  readonly prefix: string;
  readonly enabled: boolean;
  readonly expiration: number | null;
  readonly noncurrent_version_expiration: number | null;
  readonly created_at: string;
  readonly updated_at: string;
};
