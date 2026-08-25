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
  /**
   * Whether this role may create and withdraw share and embed links.
   *
   * Distinct from `manage_objects`: one authority changes what OES stores, the
   * other decides who outside OES can read it.
   */
  readonly manage_sharing: boolean;
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

/**
 * What a storage consistency scan found.
 *
 * `metadata_without_data` is the serious category: an object OES believes it
 * has whose bytes are gone. The other categories are reclaimable space rather
 * than lost data.
 */
/**
 * Point-in-time metric values from the management plane.
 *
 * These are the same numbers Prometheus scrapes, served behind management
 * authentication because the console never holds the scrape credential.
 * Counters are process-lifetime totals, not rates.
 */
export type SystemMetrics = {
  readonly requests: number;
  readonly errors: number;
  readonly upload_bytes: number;
  readonly download_bytes: number;
  readonly storage: {
    readonly object_count: number;
    readonly bucket_count: number;
    readonly version_count: number;
    readonly logical_bytes: number;
    readonly physical_bytes: number;
    readonly multipart_bytes: number;
  };
  /** Absent in standalone deployments. */
  readonly cluster?: {
    readonly nodes: number;
    readonly healthy: boolean;
    readonly quorum_writable: boolean;
    readonly under_replicated_objects: number;
    readonly repair_active_tasks: number;
    readonly node_capacity_bytes: number;
    readonly node_used_bytes: number;
    readonly node_available_bytes: number;
    readonly logical_bytes: number;
    readonly physical_bytes: number;
  };
};

export type StorageInspection = {
  readonly metadata_payloads_scanned: number;
  readonly data_payloads_scanned: number;
  readonly metadata_without_data: number;
  readonly data_without_metadata: number;
  readonly unknown_data_entries: number;
  readonly recognized_temporary_entries: number;
  readonly unknown_temporary_entries: number;
  /** Whether the scan stopped at its entry limit rather than completing. */
  readonly truncated: boolean;
  readonly missing_payload_samples: readonly string[];
  readonly orphan_payload_samples: readonly string[];
};

export type StorageRepairResult = {
  readonly inspection: StorageInspection;
  readonly removed_orphan_payloads: number;
  readonly dry_run: boolean;
};

export type BucketVerification = {
  readonly verified_objects: number;
  readonly failures: number;
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

/**
 * External access capabilities.
 *
 * A share link is for a person and a embed link is for a website, and they are
 * modelled separately here for the same reason they are modelled separately in
 * the backend: they differ in what they carry, who holds them, and how they are
 * delivered. Neither type ever carries its token — the URL is fetched by a
 * dedicated call at the moment it is copied.
 */
export type CapabilityStatus = 'active' | 'revoked' | 'expired' | 'exhausted';

export type SharePermission = 'view' | 'download' | 'view_and_download';

export type VersionMode = 'current' | 'pinned';

export type EmbedDisposition = 'inline' | 'attachment';

export type ShareLink = {
  readonly id: string;
  readonly label: string;
  readonly bucket: string;
  readonly key: string;
  readonly version_mode: VersionMode;
  readonly version_id: string | null;
  readonly permission: SharePermission;
  readonly status: CapabilityStatus;
  readonly password_protected: boolean;
  readonly created_by: string;
  readonly created_at: string;
  readonly expires_at: string | null;
  readonly revoked_at: string | null;
  readonly last_accessed_at: string | null;
  readonly access_count: number;
  readonly maximum_access_count: number | null;
};

export type EmbedLink = {
  readonly id: string;
  readonly label: string;
  readonly bucket: string;
  readonly key: string;
  readonly version_mode: VersionMode;
  readonly version_id: string | null;
  readonly status: CapabilityStatus;
  readonly content_type: string;
  readonly disposition: EmbedDisposition;
  readonly allowed_origins: readonly string[];
  readonly created_by: string;
  readonly created_at: string;
  readonly updated_at: string | null;
  readonly expires_at: string | null;
  readonly revoked_at: string | null;
  readonly last_accessed_at: string | null;
  readonly access_count: number;
};

/** A newly created capability. The URL is shown once, here. */
export type IssuedShare = {
  readonly share: ShareLink;
  readonly url: string;
};

export type IssuedEmbed = {
  readonly embed: EmbedLink;
  readonly url: string;
};

/**
 * The URL of an existing capability.
 *
 * `available` is false when the stored token can no longer be decrypted, which
 * happens after the deployment's master key changes. The link still works; only
 * showing it again does not, and the UI says so rather than showing nothing.
 */
export type CapabilityUrl = {
  readonly url: string | null;
  readonly available: boolean;
};

/** What this deployment permits, so the console offers only what will be accepted. */
export type SharingSettings = {
  readonly shares_enabled: boolean;
  readonly embeds_enabled: boolean;
  readonly maximum_lifetime_days: number | null;
  readonly require_expiration: boolean;
  readonly require_share_password: boolean;
  readonly maximum_access_count: number;
  readonly minimum_password_length: number;
  readonly preview_text_limit_bytes: number;
  readonly embeddable_content_types: readonly string[];
};

/** What a share page may learn about the object behind a link. */
export type PublicShare =
  | { readonly state: 'password_required' }
  | {
      readonly state: 'open';
      readonly file_name: string;
      readonly content_type: string | null;
      readonly size: number;
      readonly preview: PreviewKindName;
      readonly can_view: boolean;
      readonly can_download: boolean;
      readonly expires_at: string | null;
      readonly preview_text_limit_bytes: number;
    };

/** The server's own classification of how an object may be presented. */
export type PreviewKindName =
  'image' | 'video' | 'audio' | 'pdf' | 'text' | 'json' | 'unsafe_inline' | 'unsupported';
