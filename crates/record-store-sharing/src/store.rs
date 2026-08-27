//! Durable capability storage.
//!
//! Capabilities live in their own `redb` database beside the other durable
//! metadata, following the same pattern as credentials, events, and the audit
//! trail: one file, versioned tables, forward-only schema migration.
//!
//! Two properties drive the layout. Lookups arrive with a secret and must find
//! a record without that secret ever being stored in the clear, so the primary
//! index is keyed by the token's digest. And an access budget has to be a real
//! ceiling rather than an approximation, so consuming one is a single write
//! transaction that re-checks every condition it depends on — `redb` serializes
//! write transactions, which is what makes the count strict under concurrency
//! rather than merely usually right.

use std::{path::Path, sync::Arc};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use chrono::{DateTime, Utc};
use record_store_core::{BucketId, EmbedLinkId, ObjectKey, ShareLinkId};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    SharingError,
    model::{CapabilityStatus, EmbedLink, ShareLink},
    origin::AllowedOrigin,
    token::{CapabilityToken, TokenDigest},
};

const SHARES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("share_links.v1");
const SHARE_TOKENS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("share_tokens.v1");
const SHARE_OBJECTS: TableDefinition<&[u8], u8> = TableDefinition::new("share_objects.v1");
const EMBEDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("embed_links.v1");
const EMBED_TOKENS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("embed_tokens.v1");
const EMBED_OBJECTS: TableDefinition<&[u8], u8> = TableDefinition::new("embed_objects.v1");
const SCHEMA: TableDefinition<&str, u64> = TableDefinition::new("sharing_schema.v1");

/// Current durable capability-store format.
pub const SHARING_SCHEMA_VERSION: u64 = 1;

/// A byte that can never occur inside a UTF-8 object key, and therefore
/// separates a key from what follows it in a composite index entry without any
/// possibility of a key forging its own boundary.
const KEY_TERMINATOR: u8 = 0xFF;

/// The token, encrypted so an authorized administrator can copy the link again.
///
/// A capability URL has to be re-copyable: an embed is pasted into a site and
/// re-pasted when the site changes, and a share is forwarded again when the
/// first message is lost. Storing the token in the clear to allow that would be
/// indefensible, and storing only a hash would mean the link can never be shown
/// again. Envelope encryption under the deployment's master key is the same
/// answer Record Store already gives for service-account secrets and webhook signing
/// keys: a stolen database alone yields nothing, and a compromise that also
/// yields the master key has already lost far more than these tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SealedToken {
    version: u8,
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredShare {
    link: ShareLink,
    token: SealedToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEmbed {
    link: EmbedLink,
    token: SealedToken,
}

/// Why a capability could not authorize an access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessRefusal {
    /// No capability matched the presented token.
    Unknown,
    /// The capability exists but is no longer usable.
    NotUsable(CapabilityStatus),
}

/// Durable store for share and embed capabilities.
#[derive(Clone)]
pub struct CapabilityStore {
    database: Arc<Database>,
    encryption_key: Arc<Zeroizing<[u8; 32]>>,
    /// How stale a recorded access time may become before a read path pays for
    /// a write to refresh it.
    telemetry_interval: chrono::Duration,
}

impl std::fmt::Debug for CapabilityStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CapabilityStore")
            .finish_non_exhaustive()
    }
}

impl CapabilityStore {
    /// Opens the capability database, creating and migrating tables as needed.
    ///
    /// `key_material` is the deployment's master key when one is configured and
    /// the root secret otherwise, matching how the credential store derives its
    /// own encryption key. Verification never depends on it: a capability keeps
    /// working across a key change, and only the ability to redisplay its URL is
    /// lost — which the API reports rather than hides.
    pub async fn open(path: impl AsRef<Path>, key_material: &[u8]) -> Result<Self, SharingError> {
        Self::open_with_telemetry_interval(path, key_material, chrono::Duration::seconds(60)).await
    }

    /// Opens the store with an explicit access-telemetry write interval.
    pub async fn open_with_telemetry_interval(
        path: impl AsRef<Path>,
        key_material: &[u8],
        telemetry_interval: chrono::Duration,
    ) -> Result<Self, SharingError> {
        let path = path.as_ref().to_path_buf();
        let encryption_key = derive_encryption_key(key_material)?;
        let database = tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(SharingError::Directory)?;
            }
            let database = Database::create(path).map_err(|error| backend("open", error))?;
            initialize_schema(&database)?;
            Ok::<_, SharingError>(database)
        })
        .await??;
        Ok(Self {
            database: Arc::new(database),
            encryption_key: Arc::new(Zeroizing::new(encryption_key)),
            telemetry_interval,
        })
    }

    /// Stores a new share and its token in one transaction.
    pub async fn create_share(
        &self,
        link: ShareLink,
        token: &CapabilityToken,
    ) -> Result<(), SharingError> {
        let sealed = self.seal(token, link.id.as_uuid())?;
        let digest = token.digest();
        let record = serde_json::to_vec(&StoredShare {
            link: link.clone(),
            token: sealed,
        })?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin create share", error))?;
            {
                let mut tokens = write
                    .open_table(SHARE_TOKENS)
                    .map_err(|error| backend("open share tokens", error))?;
                if tokens
                    .get(digest.as_bytes().as_slice())
                    .map_err(|error| backend("read share token", error))?
                    .is_some()
                {
                    return Err(SharingError::TokenCollision);
                }
                tokens
                    .insert(
                        digest.as_bytes().as_slice(),
                        link.id.as_uuid().as_bytes().as_slice(),
                    )
                    .map_err(|error| backend("write share token", error))?;
            }
            {
                let mut shares = write
                    .open_table(SHARES)
                    .map_err(|error| backend("open shares", error))?;
                shares
                    .insert(link.id.as_uuid().as_bytes().as_slice(), record.as_slice())
                    .map_err(|error| backend("write share", error))?;
            }
            {
                let mut index = write
                    .open_table(SHARE_OBJECTS)
                    .map_err(|error| backend("open share index", error))?;
                index
                    .insert(
                        object_index_key(
                            link.target.bucket_id,
                            &link.target.key,
                            link.id.as_uuid(),
                        )
                        .as_slice(),
                        &1_u8,
                    )
                    .map_err(|error| backend("write share index", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit create share", error))
        })
        .await?
    }

    /// Returns one share by its non-secret identifier.
    pub async fn get_share(&self, id: ShareLinkId) -> Result<Option<ShareLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            read_record::<StoredShare>(&database, SHARES, id.as_uuid().as_bytes(), "read share")
                .map(|stored| stored.map(|stored| stored.link))
        })
        .await?
    }

    /// Returns the share a presented token names, without consuming anything.
    pub async fn resolve_share(
        &self,
        digest: TokenDigest,
    ) -> Result<Option<ShareLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let Some(id) = lookup_token(&database, SHARE_TOKENS, digest, "share")? else {
                return Ok(None);
            };
            read_record::<StoredShare>(&database, SHARES, id.as_bytes(), "read share")
                .map(|stored| stored.map(|stored| stored.link))
        })
        .await?
    }

    /// Lists the shares that target one object, newest first.
    pub async fn list_shares_for_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Vec<ShareLink>, SharingError> {
        let prefix = object_index_prefix(bucket_id, key);
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let ids = scan_index(&database, SHARE_OBJECTS, &prefix, "share index")?;
            let mut links = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(stored) =
                    read_record::<StoredShare>(&database, SHARES, id.as_bytes(), "read share")?
                {
                    links.push(stored.link);
                }
            }
            links.sort_by_key(|link| std::cmp::Reverse(link.created_at));
            Ok(links)
        })
        .await?
    }

    /// Returns every share, newest first. Used for operational counts.
    pub async fn list_shares(&self) -> Result<Vec<ShareLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let mut links = scan_records::<StoredShare>(&database, SHARES, "scan shares")?
                .into_iter()
                .map(|stored| stored.link)
                .collect::<Vec<_>>();
            links.sort_by_key(|link| std::cmp::Reverse(link.created_at));
            Ok(links)
        })
        .await?
    }

    /// Withdraws a share. Revocation is idempotent and immediately authoritative.
    pub async fn revoke_share(
        &self,
        id: ShareLinkId,
        at: DateTime<Utc>,
    ) -> Result<Option<ShareLink>, SharingError> {
        self.mutate_share(id, move |link| {
            if link.revoked_at.is_none() {
                link.revoked_at = Some(at);
            }
        })
        .await
    }

    /// Permanently removes a share record and its token entry.
    pub async fn delete_share(&self, id: ShareLinkId) -> Result<bool, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin delete share", error))?;
            let removed: Option<Vec<u8>> = {
                let mut shares = write
                    .open_table(SHARES)
                    .map_err(|error| backend("open shares", error))?;
                shares
                    .remove(id.as_uuid().as_bytes().as_slice())
                    .map_err(|error| backend("remove share", error))?
                    .map(|value| value.value().to_vec())
            };
            let Some(stored) = removed
                .map(|bytes| serde_json::from_slice::<StoredShare>(&bytes))
                .transpose()?
            else {
                write
                    .commit()
                    .map_err(|error| backend("commit delete share", error))?;
                return Ok(false);
            };
            {
                let mut tokens = write
                    .open_table(SHARE_TOKENS)
                    .map_err(|error| backend("open share tokens", error))?;
                // The digest is not recoverable from the record, so the token
                // index is swept for the entry pointing at this identifier.
                let stale = tokens
                    .iter()
                    .map_err(|error| backend("iterate share tokens", error))?
                    .filter_map(|entry| entry.ok())
                    .find(|(_, value)| value.value() == stored.link.id.as_uuid().as_bytes())
                    .map(|(key, _)| key.value().to_vec());
                if let Some(key) = stale {
                    tokens
                        .remove(key.as_slice())
                        .map_err(|error| backend("remove share token", error))?;
                }
            }
            {
                let mut index = write
                    .open_table(SHARE_OBJECTS)
                    .map_err(|error| backend("open share index", error))?;
                index
                    .remove(
                        object_index_key(
                            stored.link.target.bucket_id,
                            &stored.link.target.key,
                            stored.link.id.as_uuid(),
                        )
                        .as_slice(),
                    )
                    .map_err(|error| backend("remove share index", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit delete share", error))?;
            Ok(true)
        })
        .await?
    }

    /// Consumes one unit of a share's access budget, atomically.
    ///
    /// Every condition is re-evaluated inside the write transaction rather than
    /// trusted from an earlier read. That is what makes a maximum access count a
    /// genuine ceiling: two simultaneous requests against a share with one
    /// remaining use serialize here, and exactly one of them is granted.
    pub async fn consume_share_access(
        &self,
        id: ShareLinkId,
        now: DateTime<Utc>,
    ) -> Result<Result<ShareLink, AccessRefusal>, SharingError> {
        let telemetry_interval = self.telemetry_interval;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin share access", error))?;
            let existing = read_in_transaction::<StoredShare>(
                &write,
                SHARES,
                id.as_uuid().as_bytes(),
                "read share",
            )?;
            let Some(mut stored) = existing else {
                write
                    .commit()
                    .map_err(|error| backend("commit share access", error))?;
                return Ok(Err(AccessRefusal::Unknown));
            };
            let status = stored.link.status(now);
            if !status.usable() {
                write
                    .commit()
                    .map_err(|error| backend("commit share access", error))?;
                return Ok(Err(AccessRefusal::NotUsable(status)));
            }
            let budgeted = stored.link.maximum_access_count.is_some();
            let stale = stored
                .link
                .last_accessed_at
                .is_none_or(|last| now.signed_duration_since(last) >= telemetry_interval);
            stored.link.access_count = stored.link.access_count.saturating_add(1);
            stored.link.last_accessed_at = Some(now);
            // A budgeted share must be written on every access, because the
            // count is load-bearing. An unbudgeted one records only a coarse
            // last-seen time, so a media player seeking through a file does not
            // turn a read path into one write per range.
            if budgeted || stale {
                let encoded = serde_json::to_vec(&stored)?;
                let mut shares = write
                    .open_table(SHARES)
                    .map_err(|error| backend("open shares", error))?;
                shares
                    .insert(id.as_uuid().as_bytes().as_slice(), encoded.as_slice())
                    .map_err(|error| backend("write share", error))?;
            }
            let link = stored.link;
            write
                .commit()
                .map_err(|error| backend("commit share access", error))?;
            Ok(Ok(link))
        })
        .await?
    }

    /// Decrypts a share's token so an administrator can copy the link again.
    ///
    /// Returns `None` when the record cannot be decrypted with the current key,
    /// which happens after a master-key change. The capability itself is
    /// unaffected; only its redisplay is.
    pub async fn reveal_share_token(
        &self,
        id: ShareLinkId,
    ) -> Result<Option<CapabilityToken>, SharingError> {
        let database = Arc::clone(&self.database);
        let key = Arc::clone(&self.encryption_key);
        tokio::task::spawn_blocking(move || {
            let Some(stored) = read_record::<StoredShare>(
                &database,
                SHARES,
                id.as_uuid().as_bytes(),
                "read share",
            )?
            else {
                return Ok(None);
            };
            Ok(unseal(&stored.token, id.as_uuid(), &key))
        })
        .await?
    }

    async fn mutate_share(
        &self,
        id: ShareLinkId,
        change: impl FnOnce(&mut ShareLink) + Send + 'static,
    ) -> Result<Option<ShareLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin update share", error))?;
            let existing = read_in_transaction::<StoredShare>(
                &write,
                SHARES,
                id.as_uuid().as_bytes(),
                "read share",
            )?;
            let updated = match existing {
                Some(mut stored) => {
                    change(&mut stored.link);
                    let encoded = serde_json::to_vec(&stored)?;
                    {
                        let mut shares = write
                            .open_table(SHARES)
                            .map_err(|error| backend("open shares", error))?;
                        shares
                            .insert(id.as_uuid().as_bytes().as_slice(), encoded.as_slice())
                            .map_err(|error| backend("write share", error))?;
                    }
                    Some(stored.link)
                }
                None => None,
            };
            write
                .commit()
                .map_err(|error| backend("commit update share", error))?;
            Ok(updated)
        })
        .await?
    }

    /// Stores a new embed and its token in one transaction.
    pub async fn create_embed(
        &self,
        link: EmbedLink,
        token: &CapabilityToken,
    ) -> Result<(), SharingError> {
        let sealed = self.seal(token, link.id.as_uuid())?;
        let digest = token.digest();
        let record = serde_json::to_vec(&StoredEmbed {
            link: link.clone(),
            token: sealed,
        })?;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin create embed", error))?;
            {
                let mut tokens = write
                    .open_table(EMBED_TOKENS)
                    .map_err(|error| backend("open embed tokens", error))?;
                if tokens
                    .get(digest.as_bytes().as_slice())
                    .map_err(|error| backend("read embed token", error))?
                    .is_some()
                {
                    return Err(SharingError::TokenCollision);
                }
                tokens
                    .insert(
                        digest.as_bytes().as_slice(),
                        link.id.as_uuid().as_bytes().as_slice(),
                    )
                    .map_err(|error| backend("write embed token", error))?;
            }
            {
                let mut embeds = write
                    .open_table(EMBEDS)
                    .map_err(|error| backend("open embeds", error))?;
                embeds
                    .insert(link.id.as_uuid().as_bytes().as_slice(), record.as_slice())
                    .map_err(|error| backend("write embed", error))?;
            }
            {
                let mut index = write
                    .open_table(EMBED_OBJECTS)
                    .map_err(|error| backend("open embed index", error))?;
                index
                    .insert(
                        object_index_key(
                            link.target.bucket_id,
                            &link.target.key,
                            link.id.as_uuid(),
                        )
                        .as_slice(),
                        &1_u8,
                    )
                    .map_err(|error| backend("write embed index", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit create embed", error))
        })
        .await?
    }

    /// Returns one embed by its non-secret identifier.
    pub async fn get_embed(&self, id: EmbedLinkId) -> Result<Option<EmbedLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            read_record::<StoredEmbed>(&database, EMBEDS, id.as_uuid().as_bytes(), "read embed")
                .map(|stored| stored.map(|stored| stored.link))
        })
        .await?
    }

    /// Returns the embed a presented token names.
    pub async fn resolve_embed(
        &self,
        digest: TokenDigest,
    ) -> Result<Option<EmbedLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let Some(id) = lookup_token(&database, EMBED_TOKENS, digest, "embed")? else {
                return Ok(None);
            };
            read_record::<StoredEmbed>(&database, EMBEDS, id.as_bytes(), "read embed")
                .map(|stored| stored.map(|stored| stored.link))
        })
        .await?
    }

    /// Lists the embeds that target one object, newest first.
    pub async fn list_embeds_for_object(
        &self,
        bucket_id: BucketId,
        key: &ObjectKey,
    ) -> Result<Vec<EmbedLink>, SharingError> {
        let prefix = object_index_prefix(bucket_id, key);
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let ids = scan_index(&database, EMBED_OBJECTS, &prefix, "embed index")?;
            let mut links = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(stored) =
                    read_record::<StoredEmbed>(&database, EMBEDS, id.as_bytes(), "read embed")?
                {
                    links.push(stored.link);
                }
            }
            links.sort_by_key(|link| std::cmp::Reverse(link.created_at));
            Ok(links)
        })
        .await?
    }

    /// Returns every embed, newest first. Used for operational counts.
    pub async fn list_embeds(&self) -> Result<Vec<EmbedLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let mut links = scan_records::<StoredEmbed>(&database, EMBEDS, "scan embeds")?
                .into_iter()
                .map(|stored| stored.link)
                .collect::<Vec<_>>();
            links.sort_by_key(|link| std::cmp::Reverse(link.created_at));
            Ok(links)
        })
        .await?
    }

    /// Withdraws an embed.
    pub async fn revoke_embed(
        &self,
        id: EmbedLinkId,
        at: DateTime<Utc>,
    ) -> Result<Option<EmbedLink>, SharingError> {
        self.mutate_embed(id, move |link| {
            if link.revoked_at.is_none() {
                link.revoked_at = Some(at);
            }
            link.updated_at = Some(at);
        })
        .await
    }

    /// Replaces an embed's origin allowlist.
    pub async fn set_embed_origins(
        &self,
        id: EmbedLinkId,
        origins: Vec<AllowedOrigin>,
        at: DateTime<Utc>,
    ) -> Result<Option<EmbedLink>, SharingError> {
        self.mutate_embed(id, move |link| {
            link.allowed_origins = origins;
            link.updated_at = Some(at);
        })
        .await
    }

    /// Records that an embed served bytes, at most once per telemetry interval.
    ///
    /// An embed's counter is telemetry, never a limit, so it must not make a
    /// read path pay for a durable write on every range request a video player
    /// issues. The record is refreshed only once the previous one has aged past
    /// the interval, which keeps "last used" useful without turning an asset URL
    /// into a write amplifier.
    pub async fn record_embed_access(
        &self,
        id: EmbedLinkId,
        now: DateTime<Utc>,
    ) -> Result<(), SharingError> {
        let interval = self.telemetry_interval;
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let stale = {
                let Some(stored) = read_record::<StoredEmbed>(
                    &database,
                    EMBEDS,
                    id.as_uuid().as_bytes(),
                    "read embed",
                )?
                else {
                    return Ok(());
                };
                stored
                    .link
                    .last_accessed_at
                    .is_none_or(|last| now.signed_duration_since(last) >= interval)
            };
            if !stale {
                return Ok(());
            }
            let write = database
                .begin_write()
                .map_err(|error| backend("begin embed telemetry", error))?;
            let existing = read_in_transaction::<StoredEmbed>(
                &write,
                EMBEDS,
                id.as_uuid().as_bytes(),
                "read embed",
            )?;
            if let Some(mut stored) = existing {
                stored.link.last_accessed_at = Some(now);
                stored.link.access_count = stored.link.access_count.saturating_add(1);
                let encoded = serde_json::to_vec(&stored)?;
                let mut embeds = write
                    .open_table(EMBEDS)
                    .map_err(|error| backend("open embeds", error))?;
                embeds
                    .insert(id.as_uuid().as_bytes().as_slice(), encoded.as_slice())
                    .map_err(|error| backend("write embed", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit embed telemetry", error))
        })
        .await?
    }

    /// Decrypts an embed's token so its URL can be copied again.
    pub async fn reveal_embed_token(
        &self,
        id: EmbedLinkId,
    ) -> Result<Option<CapabilityToken>, SharingError> {
        let database = Arc::clone(&self.database);
        let key = Arc::clone(&self.encryption_key);
        tokio::task::spawn_blocking(move || {
            let Some(stored) = read_record::<StoredEmbed>(
                &database,
                EMBEDS,
                id.as_uuid().as_bytes(),
                "read embed",
            )?
            else {
                return Ok(None);
            };
            Ok(unseal(&stored.token, id.as_uuid(), &key))
        })
        .await?
    }

    /// Permanently removes an embed record and its token entry.
    pub async fn delete_embed(&self, id: EmbedLinkId) -> Result<bool, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin delete embed", error))?;
            let removed: Option<Vec<u8>> = {
                let mut embeds = write
                    .open_table(EMBEDS)
                    .map_err(|error| backend("open embeds", error))?;
                embeds
                    .remove(id.as_uuid().as_bytes().as_slice())
                    .map_err(|error| backend("remove embed", error))?
                    .map(|value| value.value().to_vec())
            };
            let Some(stored) = removed
                .map(|bytes| serde_json::from_slice::<StoredEmbed>(&bytes))
                .transpose()?
            else {
                write
                    .commit()
                    .map_err(|error| backend("commit delete embed", error))?;
                return Ok(false);
            };
            {
                let mut tokens = write
                    .open_table(EMBED_TOKENS)
                    .map_err(|error| backend("open embed tokens", error))?;
                let stale = tokens
                    .iter()
                    .map_err(|error| backend("iterate embed tokens", error))?
                    .filter_map(|entry| entry.ok())
                    .find(|(_, value)| value.value() == stored.link.id.as_uuid().as_bytes())
                    .map(|(key, _)| key.value().to_vec());
                if let Some(key) = stale {
                    tokens
                        .remove(key.as_slice())
                        .map_err(|error| backend("remove embed token", error))?;
                }
            }
            {
                let mut index = write
                    .open_table(EMBED_OBJECTS)
                    .map_err(|error| backend("open embed index", error))?;
                index
                    .remove(
                        object_index_key(
                            stored.link.target.bucket_id,
                            &stored.link.target.key,
                            stored.link.id.as_uuid(),
                        )
                        .as_slice(),
                    )
                    .map_err(|error| backend("remove embed index", error))?;
            }
            write
                .commit()
                .map_err(|error| backend("commit delete embed", error))?;
            Ok(true)
        })
        .await?
    }

    async fn mutate_embed(
        &self,
        id: EmbedLinkId,
        change: impl FnOnce(&mut EmbedLink) + Send + 'static,
    ) -> Result<Option<EmbedLink>, SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let write = database
                .begin_write()
                .map_err(|error| backend("begin update embed", error))?;
            let existing = read_in_transaction::<StoredEmbed>(
                &write,
                EMBEDS,
                id.as_uuid().as_bytes(),
                "read embed",
            )?;
            let updated = match existing {
                Some(mut stored) => {
                    change(&mut stored.link);
                    let encoded = serde_json::to_vec(&stored)?;
                    {
                        let mut embeds = write
                            .open_table(EMBEDS)
                            .map_err(|error| backend("open embeds", error))?;
                        embeds
                            .insert(id.as_uuid().as_bytes().as_slice(), encoded.as_slice())
                            .map_err(|error| backend("write embed", error))?;
                    }
                    Some(stored.link)
                }
                None => None,
            };
            write
                .commit()
                .map_err(|error| backend("commit update embed", error))?;
            Ok(updated)
        })
        .await?
    }

    /// Confirms the durable store is reachable.
    pub async fn check_ready(&self) -> Result<(), SharingError> {
        let database = Arc::clone(&self.database);
        tokio::task::spawn_blocking(move || {
            let read = database
                .begin_read()
                .map_err(|error| backend("begin readiness", error))?;
            read.open_table(SHARES)
                .map(|_| ())
                .map_err(|error| backend("open shares", error))
        })
        .await?
    }

    fn seal(&self, token: &CapabilityToken, id: Uuid) -> Result<SealedToken, SharingError> {
        let cipher = Aes256Gcm::new_from_slice(self.encryption_key.as_slice())
            .map_err(|_| SharingError::Cryptography)?;
        let mut nonce = [0_u8; 12];
        getrandom::fill(&mut nonce).map_err(|_| SharingError::EntropyUnavailable)?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: token.expose().as_bytes(),
                    aad: id.as_bytes(),
                },
            )
            .map_err(|_| SharingError::Cryptography)?;
        Ok(SealedToken {
            version: 1,
            nonce,
            ciphertext,
        })
    }
}

fn unseal(sealed: &SealedToken, id: Uuid, key: &[u8; 32]) -> Option<CapabilityToken> {
    if sealed.version != 1 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&sealed.nonce),
            Payload {
                msg: &sealed.ciphertext,
                aad: id.as_bytes(),
            },
        )
        .ok()?;
    let plaintext = Zeroizing::new(plaintext);
    CapabilityToken::parse(std::str::from_utf8(&plaintext).ok()?)
}

fn derive_encryption_key(material: &[u8]) -> Result<[u8; 32], SharingError> {
    let derivation = hkdf::Hkdf::<Sha256>::new(Some(b"capability-store-v1"), material);
    let mut key = [0_u8; 32];
    derivation
        .expand(b"capability-token-encryption-key", &mut key)
        .map_err(|_| SharingError::Cryptography)?;
    Ok(key)
}

fn object_index_prefix(bucket_id: BucketId, key: &ObjectKey) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(16 + key.as_str().len() + 1);
    prefix.extend_from_slice(bucket_id.as_uuid().as_bytes());
    prefix.extend_from_slice(key.as_str().as_bytes());
    prefix.push(KEY_TERMINATOR);
    prefix
}

fn object_index_key(bucket_id: BucketId, key: &ObjectKey, id: Uuid) -> Vec<u8> {
    let mut composite = object_index_prefix(bucket_id, key);
    composite.extend_from_slice(id.as_bytes());
    composite
}

fn lookup_token(
    database: &Database,
    table: TableDefinition<&[u8], &[u8]>,
    digest: TokenDigest,
    what: &'static str,
) -> Result<Option<Uuid>, SharingError> {
    let read = database
        .begin_read()
        .map_err(|error| backend("begin token lookup", error))?;
    let tokens = read
        .open_table(table)
        .map_err(|error| backend("open token index", error))?;
    let Some(value) = tokens
        .get(digest.as_bytes().as_slice())
        .map_err(|error| backend("read token index", error))?
    else {
        return Ok(None);
    };
    let bytes: [u8; 16] = value
        .value()
        .try_into()
        .map_err(|_| SharingError::Database {
            operation: "read token index",
            reason: format!("{what} token index holds a malformed identifier"),
        })?;
    Ok(Some(Uuid::from_bytes(bytes)))
}

fn read_record<T: DeserializeOwned>(
    database: &Database,
    table: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    operation: &'static str,
) -> Result<Option<T>, SharingError> {
    let read = database
        .begin_read()
        .map_err(|error| backend(operation, error))?;
    let table = read
        .open_table(table)
        .map_err(|error| backend(operation, error))?;
    table
        .get(key)
        .map_err(|error| backend(operation, error))?
        .map(|value| serde_json::from_slice::<T>(value.value()))
        .transpose()
        .map_err(SharingError::from)
}

/// Reads and decodes one record inside an open write transaction.
///
/// The decoded value is owned before the table handle is dropped, so the caller
/// can reopen the same table to write without holding a borrow across the two.
fn read_in_transaction<T: DeserializeOwned>(
    write: &redb::WriteTransaction,
    table: TableDefinition<&[u8], &[u8]>,
    key: &[u8],
    operation: &'static str,
) -> Result<Option<T>, SharingError> {
    let raw = {
        let table = write
            .open_table(table)
            .map_err(|error| backend(operation, error))?;
        table
            .get(key)
            .map_err(|error| backend(operation, error))?
            .map(|value| value.value().to_vec())
    };
    raw.map(|bytes| serde_json::from_slice::<T>(&bytes))
        .transpose()
        .map_err(SharingError::from)
}

fn scan_records<T: DeserializeOwned>(
    database: &Database,
    table: TableDefinition<&[u8], &[u8]>,
    operation: &'static str,
) -> Result<Vec<T>, SharingError> {
    let read = database
        .begin_read()
        .map_err(|error| backend(operation, error))?;
    let table = read
        .open_table(table)
        .map_err(|error| backend(operation, error))?;
    let mut records = Vec::new();
    for entry in table.iter().map_err(|error| backend(operation, error))? {
        let (_, value) = entry.map_err(|error| backend(operation, error))?;
        records.push(serde_json::from_slice(value.value())?);
    }
    Ok(records)
}

fn scan_index(
    database: &Database,
    table: TableDefinition<&[u8], u8>,
    prefix: &[u8],
    operation: &'static str,
) -> Result<Vec<Uuid>, SharingError> {
    let read = database
        .begin_read()
        .map_err(|error| backend(operation, error))?;
    let table = read
        .open_table(table)
        .map_err(|error| backend(operation, error))?;
    let mut ids = Vec::new();
    for entry in table
        .range(prefix..)
        .map_err(|error| backend(operation, error))?
    {
        let (key, _) = entry.map_err(|error| backend(operation, error))?;
        let key = key.value();
        if !key.starts_with(prefix) {
            break;
        }
        if let Ok(bytes) = <[u8; 16]>::try_from(&key[prefix.len()..]) {
            ids.push(Uuid::from_bytes(bytes));
        }
    }
    Ok(ids)
}

fn initialize_schema(database: &Database) -> Result<(), SharingError> {
    let write = database
        .begin_write()
        .map_err(|error| backend("initialize", error))?;
    for table in [SHARES, SHARE_TOKENS, EMBEDS, EMBED_TOKENS] {
        write
            .open_table(table)
            .map_err(|error| backend("initialize capability table", error))?;
    }
    for table in [SHARE_OBJECTS, EMBED_OBJECTS] {
        write
            .open_table(table)
            .map_err(|error| backend("initialize capability index", error))?;
    }
    let version = {
        let table = write
            .open_table(SCHEMA)
            .map_err(|error| backend("open schema", error))?;
        table
            .get("sharing")
            .map_err(|error| backend("read schema", error))?
            .map_or(0, |value| value.value())
    };
    if version > SHARING_SCHEMA_VERSION {
        return Err(SharingError::Database {
            operation: "schema compatibility",
            reason: format!(
                "capability schema {version} is newer than supported schema {SHARING_SCHEMA_VERSION}"
            ),
        });
    }
    if version < SHARING_SCHEMA_VERSION {
        let mut table = write
            .open_table(SCHEMA)
            .map_err(|error| backend("open schema", error))?;
        table
            .insert("sharing", &SHARING_SCHEMA_VERSION)
            .map_err(|error| backend("write schema", error))?;
    }
    write
        .commit()
        .map_err(|error| backend("commit initialization", error))
}

fn backend(operation: &'static str, error: impl std::fmt::Display) -> SharingError {
    SharingError::Database {
        operation,
        reason: error.to_string(),
    }
}
