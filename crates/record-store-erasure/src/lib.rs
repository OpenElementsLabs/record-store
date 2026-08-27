//! Bounded, integrity-checked Reed-Solomon stripes for Record Store.
//!
//! The external codec is deliberately hidden behind [`ErasureCodec`]. Object
//! storage owns identifiers, checksums, stripe layout, async isolation, and
//! recovery policy; the dependency only performs coding arithmetic.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::StreamExt;
use md5::{Digest as _, Md5};
use record_store_core::{
    ByteRange, Checksum, ETag, ErasureProfile, ResolvedByteRange, ShardId, ShardIndex, ShardKind,
    ShardState, StripeId,
};
use record_store_storage::UploadStream;
use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use tokio::sync::Semaphore;

/// Storage-format discriminator for the stripe layout in this module.
pub const ERASURE_FORMAT_VERSION: u16 = 1;
/// Logical object bytes encoded in one full stripe (8 MiB).
pub const STRIPE_DATA_BYTES: usize = 8 * 1024 * 1024;
/// Alignment that preserves codec compatibility across supported crate versions.
pub const SHARD_ALIGNMENT: usize = 64;

/// Failures produced before any erasure result may be trusted.
#[derive(Debug, Error)]
pub enum ErasureError {
    /// The chosen profile or stripe is not supported by the storage format.
    #[error("invalid erasure input: {0}")]
    InvalidInput(String),
    /// A shard descriptor does not match the authoritative manifest.
    #[error("invalid shard identity: {0}")]
    InvalidShardIdentity(String),
    /// The same shard index was supplied more than once.
    #[error("duplicate shard index {0}")]
    DuplicateShard(u16),
    /// Too few independently valid shards remain.
    #[error("stripe is unrecoverable: required {required} valid shards, found {available}")]
    Unrecoverable { required: u8, available: u8 },
    /// Coding or reconstructed-content validation failed.
    #[error("erasure integrity verification failed: {0}")]
    Integrity(String),
    /// The selected Reed-Solomon implementation rejected the operation.
    #[error("Reed-Solomon operation failed: {0}")]
    Codec(String),
    /// The client upload stream failed.
    #[error("upload stream failed: {0}")]
    Upload(#[source] std::io::Error),
    /// The bounded CPU executor is shutting down.
    #[error("erasure CPU executor is unavailable")]
    ExecutorUnavailable,
    /// A blocking codec task panicked or was cancelled.
    #[error("erasure CPU task failed: {0}")]
    CpuTask(String),
}

/// Authoritative identity and checksum for one shard in one stripe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardManifest {
    /// Stable logical shard identity.
    pub id: ShardId,
    /// Owning stripe.
    pub stripe_id: StripeId,
    /// Zero-based position in the systematic codeword.
    pub index: ShardIndex,
    /// Data or parity role.
    pub kind: ShardKind,
    /// Exact encoded byte count.
    pub size: u64,
    /// SHA-256 calculated over this shard alone.
    pub checksum: Checksum,
    /// Persisted lifecycle state; only healthy shards are readable.
    pub state: ShardState,
}

/// Versioned layout of one bounded object stripe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripeManifest {
    /// Storage format that defines padding and systematic byte ordering.
    pub format_version: u16,
    /// Stable stripe identity.
    pub id: StripeId,
    /// Zero-based stripe number within the object.
    pub ordinal: u64,
    /// Logical object offset of this stripe.
    pub logical_offset: u64,
    /// Original, unpadded bytes in this stripe.
    pub logical_size: u64,
    /// Equal encoded byte count of every shard.
    pub shard_size: u64,
    /// Actual coding profile used, independent of current bucket policy.
    pub profile: ErasureProfile,
    /// All `K + M` shard records in index order.
    pub shards: Vec<ShardManifest>,
}

impl StripeManifest {
    /// Validates manifest structure before using any caller-provided bytes.
    pub fn validate(&self) -> Result<(), ErasureError> {
        if self.format_version != ERASURE_FORMAT_VERSION {
            return Err(ErasureError::InvalidInput(format!(
                "unsupported erasure format {}",
                self.format_version
            )));
        }
        if self.shard_size == 0
            || !self
                .shard_size
                .is_multiple_of(u64::try_from(SHARD_ALIGNMENT).unwrap_or(64))
        {
            return Err(ErasureError::InvalidInput(
                "shard size must be a non-zero 64-byte multiple".into(),
            ));
        }
        let expected = usize::from(self.profile.total_shards());
        if self.shards.len() != expected {
            return Err(ErasureError::InvalidInput(format!(
                "manifest has {} shards, expected {expected}",
                self.shards.len()
            )));
        }
        for (raw_index, shard) in self.shards.iter().enumerate() {
            let index = u16::try_from(raw_index)
                .map_err(|_| ErasureError::InvalidInput("shard index overflow".into()))?;
            if shard.stripe_id != self.id
                || shard.index.get() != index
                || shard.kind != self.profile.kind(shard.index)
                || shard.size != self.shard_size
            {
                return Err(ErasureError::InvalidShardIdentity(format!(
                    "manifest shard {raw_index} does not match its stripe, index, kind, or size"
                )));
            }
        }
        let capacity = self
            .shard_size
            .checked_mul(u64::from(self.profile.data_shards()))
            .ok_or_else(|| ErasureError::InvalidInput("stripe capacity overflow".into()))?;
        if self.logical_size > capacity {
            return Err(ErasureError::InvalidInput(
                "logical stripe size exceeds encoded capacity".into(),
            ));
        }
        Ok(())
    }

    /// Returns one authoritative shard record by index.
    #[must_use]
    pub fn shard(&self, index: ShardIndex) -> Option<&ShardManifest> {
        self.shards.get(usize::from(index.get()))
    }
}

/// Encoded bytes paired with their verified manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedShard {
    /// Authoritative identity and checksum.
    pub manifest: ShardManifest,
    /// Padded shard bytes.
    pub bytes: Vec<u8>,
}

/// One encoded stripe ready to be streamed to distinct targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedStripe {
    /// Durable layout metadata.
    pub manifest: StripeManifest,
    /// All encoded shards in manifest order.
    pub shards: Vec<EncodedShard>,
}

/// A shard read from storage. Its descriptor must match the manifest exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableShard {
    /// Identity supplied by the storage record.
    pub id: ShardId,
    /// Owning stripe supplied by the storage record.
    pub stripe_id: StripeId,
    /// Position supplied by the storage record.
    pub index: ShardIndex,
    /// Bytes read from the target.
    pub bytes: Vec<u8>,
}

/// Successfully decoded original bytes and the damage encountered on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedStripe {
    /// Original bytes with codec padding removed.
    pub bytes: Vec<u8>,
    /// Shards that were not supplied.
    pub missing: Vec<ShardIndex>,
    /// Supplied shards whose checksum was invalid.
    pub corrupt: Vec<ShardIndex>,
    /// Whether parity reconstruction was needed.
    pub reconstructed: bool,
}

/// Synchronous coding boundary owned by Record Store.
pub trait ErasureCodec: Send + Sync + 'static {
    /// Generates all parity shards from equally sized systematic data shards.
    fn encode(
        &self,
        profile: ErasureProfile,
        data: &[Vec<u8>],
    ) -> Result<Vec<Vec<u8>>, ErasureError>;

    /// Restores absent data and parity shards when at least `K` are present.
    fn reconstruct(
        &self,
        profile: ErasureProfile,
        shards: &mut [Option<Vec<u8>>],
    ) -> Result<(), ErasureError>;
}

/// Pure-Rust, runtime-SIMD implementation used by production builds.
#[derive(Debug, Default)]
pub struct ReedSolomonSimdCodec;

impl ErasureCodec for ReedSolomonSimdCodec {
    fn encode(
        &self,
        profile: ErasureProfile,
        data: &[Vec<u8>],
    ) -> Result<Vec<Vec<u8>>, ErasureError> {
        validate_data_shards(profile, data)?;
        let shard_size = data[0].len();
        let mut encoder = ReedSolomonEncoder::new(
            usize::from(profile.data_shards()),
            usize::from(profile.parity_shards()),
            shard_size,
        )
        .map_err(codec_error)?;
        for shard in data {
            encoder.add_original_shard(shard).map_err(codec_error)?;
        }
        let result = encoder.encode().map_err(codec_error)?;
        Ok(result.recovery_iter().map(ToOwned::to_owned).collect())
    }

    fn reconstruct(
        &self,
        profile: ErasureProfile,
        shards: &mut [Option<Vec<u8>>],
    ) -> Result<(), ErasureError> {
        let total = usize::from(profile.total_shards());
        if shards.len() != total {
            return Err(ErasureError::InvalidInput(format!(
                "received {} shard slots, expected {total}",
                shards.len()
            )));
        }
        let available = shards.iter().flatten().count();
        if available < usize::from(profile.data_shards()) {
            return Err(ErasureError::Unrecoverable {
                required: profile.data_shards(),
                available: u8::try_from(available).unwrap_or(u8::MAX),
            });
        }
        let shard_size = shards
            .iter()
            .flatten()
            .next()
            .ok_or(ErasureError::Unrecoverable {
                required: profile.data_shards(),
                available: 0,
            })?
            .len();
        if shard_size == 0
            || shards
                .iter()
                .flatten()
                .any(|shard| shard.len() != shard_size)
        {
            return Err(ErasureError::InvalidInput(
                "available shards must have one non-zero size".into(),
            ));
        }

        let data_count = usize::from(profile.data_shards());
        let missing_data: Vec<usize> = (0..data_count)
            .filter(|index| shards[*index].is_none())
            .collect();
        if !missing_data.is_empty() {
            let mut decoder = ReedSolomonDecoder::new(
                data_count,
                usize::from(profile.parity_shards()),
                shard_size,
            )
            .map_err(codec_error)?;
            for (index, shard) in shards.iter().enumerate() {
                let Some(bytes) = shard else { continue };
                if index < data_count {
                    decoder
                        .add_original_shard(index, bytes)
                        .map_err(codec_error)?;
                } else {
                    decoder
                        .add_recovery_shard(index - data_count, bytes)
                        .map_err(codec_error)?;
                }
            }
            let result = decoder.decode().map_err(codec_error)?;
            let restored: BTreeMap<usize, Vec<u8>> = result
                .restored_original_iter()
                .map(|(index, bytes)| (index, bytes.to_vec()))
                .collect();
            for index in missing_data {
                shards[index] = Some(restored.get(&index).cloned().ok_or_else(|| {
                    ErasureError::Codec(format!("decoder did not restore data shard {index}"))
                })?);
            }
        }

        let data: Vec<Vec<u8>> = shards[..data_count]
            .iter()
            .map(|shard| {
                shard.clone().ok_or_else(|| {
                    ErasureError::Codec("decoder left an original shard absent".into())
                })
            })
            .collect::<Result<_, _>>()?;
        if shards[data_count..].iter().any(Option::is_none) {
            let parity = self.encode(profile, &data)?;
            for (parity_index, generated) in parity.into_iter().enumerate() {
                let index = data_count + parity_index;
                if shards[index].is_none() {
                    shards[index] = Some(generated);
                }
            }
        }
        Ok(())
    }
}

fn codec_error(error: impl std::fmt::Display) -> ErasureError {
    ErasureError::Codec(error.to_string())
}

fn validate_data_shards(profile: ErasureProfile, data: &[Vec<u8>]) -> Result<(), ErasureError> {
    if data.len() != usize::from(profile.data_shards()) {
        return Err(ErasureError::InvalidInput(format!(
            "received {} data shards, expected {}",
            data.len(),
            profile.data_shards()
        )));
    }
    let Some(first) = data.first() else {
        return Err(ErasureError::InvalidInput("no data shards".into()));
    };
    if first.is_empty()
        || !first.len().is_multiple_of(SHARD_ALIGNMENT)
        || data.iter().any(|shard| shard.len() != first.len())
    {
        return Err(ErasureError::InvalidInput(
            "data shards must be equally sized non-zero 64-byte multiples".into(),
        ));
    }
    Ok(())
}

/// Bounded-cardinality erasure metrics. No object or shard identifier is a label.
#[derive(Debug, Default)]
pub struct ErasureMetrics {
    encode_bytes: AtomicU64,
    decode_bytes: AtomicU64,
    reconstructions: AtomicU64,
    reconstruction_failures: AtomicU64,
    degraded_objects: AtomicU64,
    unrecoverable_objects: AtomicU64,
    shards_missing: AtomicU64,
    shards_corrupt: AtomicU64,
}

/// Copyable point-in-time metrics used by Prometheus exposition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ErasureMetricsSnapshot {
    pub encode_bytes: u64,
    pub decode_bytes: u64,
    pub reconstructions: u64,
    pub reconstruction_failures: u64,
    pub degraded_objects: u64,
    pub unrecoverable_objects: u64,
    pub shards_missing: u64,
    pub shards_corrupt: u64,
}

impl ErasureMetrics {
    /// Returns a relaxed, internally consistent-enough observability snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ErasureMetricsSnapshot {
        ErasureMetricsSnapshot {
            encode_bytes: self.encode_bytes.load(Ordering::Relaxed),
            decode_bytes: self.decode_bytes.load(Ordering::Relaxed),
            reconstructions: self.reconstructions.load(Ordering::Relaxed),
            reconstruction_failures: self.reconstruction_failures.load(Ordering::Relaxed),
            degraded_objects: self.degraded_objects.load(Ordering::Relaxed),
            unrecoverable_objects: self.unrecoverable_objects.load(Ordering::Relaxed),
            shards_missing: self.shards_missing.load(Ordering::Relaxed),
            shards_corrupt: self.shards_corrupt.load(Ordering::Relaxed),
        }
    }
}

/// Async facade that bounds memory and isolates synchronous coding from Tokio.
pub struct ErasureEngine {
    codec: Arc<dyn ErasureCodec>,
    cpu: Arc<Semaphore>,
    stripe_data_bytes: usize,
    metrics: Arc<ErasureMetrics>,
}

impl ErasureEngine {
    /// Creates the production engine with a conservative CPU concurrency limit.
    #[must_use]
    pub fn new(maximum_cpu_tasks: usize) -> Self {
        Self::with_codec(
            Arc::new(ReedSolomonSimdCodec),
            maximum_cpu_tasks,
            STRIPE_DATA_BYTES,
        )
    }

    /// Injects a codec for deterministic testing while preserving all Record Store checks.
    #[must_use]
    pub fn with_codec(
        codec: Arc<dyn ErasureCodec>,
        maximum_cpu_tasks: usize,
        stripe_data_bytes: usize,
    ) -> Self {
        assert!(maximum_cpu_tasks > 0, "at least one CPU task is required");
        assert!(stripe_data_bytes > 0, "stripe size must be non-zero");
        Self {
            codec,
            cpu: Arc::new(Semaphore::new(maximum_cpu_tasks)),
            stripe_data_bytes,
            metrics: Arc::new(ErasureMetrics::default()),
        }
    }

    /// Returns shared low-cardinality metrics.
    #[must_use]
    pub fn metrics(&self) -> Arc<ErasureMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Returns the fixed logical stripe size for this storage-format writer.
    #[must_use]
    pub const fn stripe_data_bytes(&self) -> usize {
        self.stripe_data_bytes
    }

    /// Streams an upload through one bounded stripe buffer.
    ///
    /// `consume` must durably write and independently verify all shards before
    /// returning. Manifests are returned only for stripes whose consumer
    /// succeeded, so callers can atomically publish them with object metadata.
    pub async fn encode_stream<F, Fut>(
        &self,
        profile: ErasureProfile,
        mut body: UploadStream,
        mut consume: F,
    ) -> Result<EncodeStreamResult, ErasureError>
    where
        F: FnMut(EncodedStripe) -> Fut,
        Fut: Future<Output = Result<(), ErasureError>>,
    {
        let mut pending = Vec::with_capacity(self.stripe_data_bytes);
        let mut manifests = Vec::new();
        let mut object_sha = Sha256::new();
        let mut object_md5 = Md5::new();
        let mut logical_size = 0_u64;
        let mut ordinal = 0_u64;

        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(ErasureError::Upload)?;
            let mut remaining = chunk.as_ref();
            while !remaining.is_empty() {
                let available = self.stripe_data_bytes - pending.len();
                let take = available.min(remaining.len());
                let fragment = &remaining[..take];
                pending.extend_from_slice(fragment);
                object_sha.update(fragment);
                object_md5.update(fragment);
                logical_size = logical_size
                    .checked_add(u64::try_from(take).unwrap_or(u64::MAX))
                    .ok_or_else(|| ErasureError::InvalidInput("object size overflow".into()))?;
                remaining = &remaining[take..];
                if pending.len() == self.stripe_data_bytes {
                    let bytes =
                        std::mem::replace(&mut pending, Vec::with_capacity(self.stripe_data_bytes));
                    let offset = logical_size
                        .checked_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                        .ok_or_else(|| {
                            ErasureError::InvalidInput("stripe offset underflow".into())
                        })?;
                    let stripe = self.encode_one(profile, ordinal, offset, bytes).await?;
                    manifests.push(stripe.manifest.clone());
                    consume(stripe).await?;
                    ordinal = ordinal.saturating_add(1);
                }
            }
        }

        if !pending.is_empty() || ordinal == 0 {
            let offset = logical_size
                .checked_sub(u64::try_from(pending.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| ErasureError::InvalidInput("stripe offset underflow".into()))?;
            let stripe = self.encode_one(profile, ordinal, offset, pending).await?;
            manifests.push(stripe.manifest.clone());
            consume(stripe).await?;
        }

        Ok(EncodeStreamResult {
            size: logical_size,
            checksum: Checksum::sha256(object_sha.finalize().into()),
            etag: ETag::from_md5(object_md5.finalize().into()),
            stripes: manifests,
        })
    }

    /// Encodes one bounded stripe on the blocking pool under a semaphore permit.
    pub async fn encode_one(
        &self,
        profile: ErasureProfile,
        ordinal: u64,
        logical_offset: u64,
        bytes: Vec<u8>,
    ) -> Result<EncodedStripe, ErasureError> {
        if bytes.len() > self.stripe_data_bytes {
            return Err(ErasureError::InvalidInput(format!(
                "stripe contains {} bytes, limit is {}",
                bytes.len(),
                self.stripe_data_bytes
            )));
        }
        let permit = Arc::clone(&self.cpu)
            .acquire_owned()
            .await
            .map_err(|_| ErasureError::ExecutorUnavailable)?;
        let codec = Arc::clone(&self.codec);
        let logical_size = u64::try_from(bytes.len())
            .map_err(|_| ErasureError::InvalidInput("stripe size overflow".into()))?;
        let encoded = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            encode_stripe(codec.as_ref(), profile, ordinal, logical_offset, bytes)
        })
        .await
        .map_err(|error| ErasureError::CpuTask(error.to_string()))??;
        self.metrics
            .encode_bytes
            .fetch_add(logical_size, Ordering::Relaxed);
        Ok(encoded)
    }

    /// Validates and decodes a stripe on the bounded blocking pool.
    pub async fn decode_one(
        &self,
        manifest: StripeManifest,
        available: Vec<AvailableShard>,
    ) -> Result<DecodedStripe, ErasureError> {
        let permit = Arc::clone(&self.cpu)
            .acquire_owned()
            .await
            .map_err(|_| ErasureError::ExecutorUnavailable)?;
        let codec = Arc::clone(&self.codec);
        let logical_size = manifest.logical_size;
        let decoded = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            decode_stripe(codec.as_ref(), &manifest, available)
        })
        .await
        .map_err(|error| ErasureError::CpuTask(error.to_string()))?;
        match decoded {
            Ok(decoded) => {
                self.metrics
                    .decode_bytes
                    .fetch_add(logical_size, Ordering::Relaxed);
                self.metrics.shards_missing.fetch_add(
                    u64::try_from(decoded.missing.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                self.metrics.shards_corrupt.fetch_add(
                    u64::try_from(decoded.corrupt.len()).unwrap_or(u64::MAX),
                    Ordering::Relaxed,
                );
                if decoded.reconstructed {
                    self.metrics.reconstructions.fetch_add(1, Ordering::Relaxed);
                    self.metrics
                        .degraded_objects
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(decoded)
            }
            Err(error @ ErasureError::Unrecoverable { .. }) => {
                self.metrics
                    .reconstruction_failures
                    .fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .unrecoverable_objects
                    .fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

/// Metadata produced after every stripe consumer confirmed durability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeStreamResult {
    /// Total original object bytes.
    pub size: u64,
    /// End-to-end object SHA-256.
    pub checksum: Checksum,
    /// Compatibility single-part MD5 ETag.
    pub etag: ETag,
    /// Versioned stripe manifests in object order.
    pub stripes: Vec<StripeManifest>,
}

fn encode_stripe(
    codec: &dyn ErasureCodec,
    profile: ErasureProfile,
    ordinal: u64,
    logical_offset: u64,
    bytes: Vec<u8>,
) -> Result<EncodedStripe, ErasureError> {
    let logical_size = u64::try_from(bytes.len())
        .map_err(|_| ErasureError::InvalidInput("stripe size overflow".into()))?;
    let data_count = usize::from(profile.data_shards());
    let raw_shard_size = bytes.len().div_ceil(data_count).max(1);
    let shard_size = raw_shard_size.div_ceil(SHARD_ALIGNMENT) * SHARD_ALIGNMENT;
    let mut data = vec![vec![0_u8; shard_size]; data_count];
    for (index, chunk) in bytes.chunks(shard_size).enumerate() {
        data[index][..chunk.len()].copy_from_slice(chunk);
    }
    let parity = codec.encode(profile, &data)?;
    let stripe_id = StripeId::new();
    let all = data.into_iter().chain(parity).collect::<Vec<_>>();
    let mut manifests = Vec::with_capacity(all.len());
    let mut encoded = Vec::with_capacity(all.len());
    for (raw_index, shard_bytes) in all.into_iter().enumerate() {
        let index = ShardIndex::new(u16::try_from(raw_index).map_err(|_| {
            ErasureError::InvalidInput("shard index exceeds storage format".into())
        })?)
        .map_err(|error| ErasureError::InvalidInput(error.to_string()))?;
        let manifest = ShardManifest {
            id: ShardId::new(),
            stripe_id,
            index,
            kind: profile.kind(index),
            size: u64::try_from(shard_bytes.len())
                .map_err(|_| ErasureError::InvalidInput("shard size overflow".into()))?,
            checksum: checksum(&shard_bytes),
            state: ShardState::Healthy,
        };
        manifests.push(manifest.clone());
        encoded.push(EncodedShard {
            manifest,
            bytes: shard_bytes,
        });
    }
    let manifest = StripeManifest {
        format_version: ERASURE_FORMAT_VERSION,
        id: stripe_id,
        ordinal,
        logical_offset,
        logical_size,
        shard_size: u64::try_from(shard_size)
            .map_err(|_| ErasureError::InvalidInput("shard size overflow".into()))?,
        profile,
        shards: manifests,
    };
    manifest.validate()?;
    Ok(EncodedStripe {
        manifest,
        shards: encoded,
    })
}

fn decode_stripe(
    codec: &dyn ErasureCodec,
    manifest: &StripeManifest,
    available: Vec<AvailableShard>,
) -> Result<DecodedStripe, ErasureError> {
    manifest.validate()?;
    let total = usize::from(manifest.profile.total_shards());
    let mut slots = vec![None; total];
    let mut corrupt = Vec::new();
    let mut supplied = BTreeSet::new();
    for shard in available {
        let index = usize::from(shard.index.get());
        if index >= total {
            return Err(ErasureError::InvalidShardIdentity(format!(
                "index {index} is outside this profile"
            )));
        }
        if !supplied.insert(shard.index) {
            return Err(ErasureError::DuplicateShard(shard.index.get()));
        }
        let expected = &manifest.shards[index];
        if shard.id != expected.id
            || shard.stripe_id != manifest.id
            || shard.index != expected.index
        {
            return Err(ErasureError::InvalidShardIdentity(format!(
                "supplied shard at index {index} does not match the committed manifest"
            )));
        }
        if shard.bytes.len() != usize::try_from(expected.size).unwrap_or(usize::MAX)
            || checksum(&shard.bytes) != expected.checksum
        {
            corrupt.push(shard.index);
            continue;
        }
        slots[index] = Some(shard.bytes);
    }
    let missing: Vec<ShardIndex> = manifest
        .shards
        .iter()
        .filter(|shard| !supplied.contains(&shard.index))
        .map(|shard| shard.index)
        .collect();
    let valid = slots.iter().flatten().count();
    if valid < usize::from(manifest.profile.data_shards()) {
        return Err(ErasureError::Unrecoverable {
            required: manifest.profile.data_shards(),
            available: u8::try_from(valid).unwrap_or(u8::MAX),
        });
    }

    let data_count = usize::from(manifest.profile.data_shards());
    let direct = slots[..data_count].iter().all(Option::is_some);
    if !direct {
        codec.reconstruct(manifest.profile, &mut slots)?;
        for (index, bytes) in slots.iter().enumerate() {
            let bytes = bytes.as_ref().ok_or_else(|| {
                ErasureError::Integrity(format!("shard {index} remained absent after decode"))
            })?;
            if checksum(bytes) != manifest.shards[index].checksum {
                return Err(ErasureError::Integrity(format!(
                    "reconstructed shard {index} does not match its checksum"
                )));
            }
        }
    }
    let logical_size = usize::try_from(manifest.logical_size)
        .map_err(|_| ErasureError::InvalidInput("logical stripe size exceeds usize".into()))?;
    let mut bytes = Vec::with_capacity(logical_size);
    for shard in slots.into_iter().take(data_count) {
        let shard = shard.ok_or_else(|| {
            ErasureError::Integrity("healthy data path unexpectedly lacked a shard".into())
        })?;
        let remaining = logical_size.saturating_sub(bytes.len());
        bytes.extend_from_slice(&shard[..remaining.min(shard.len())]);
    }
    if bytes.len() != logical_size {
        return Err(ErasureError::Integrity(
            "decoded stripe did not contain its declared logical size".into(),
        ));
    }
    Ok(DecodedStripe {
        bytes,
        missing,
        corrupt,
        reconstructed: !direct,
    })
}

fn checksum(bytes: &[u8]) -> Checksum {
    Checksum::sha256(Sha256::digest(bytes).into())
}

/// One stripe fragment needed to satisfy a resolved object range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeRange {
    /// Stripe ordinal to fetch.
    pub stripe_ordinal: u64,
    /// Offset within decoded stripe bytes.
    pub offset: u64,
    /// Bytes to return from this stripe.
    pub length: u64,
}

/// Maps an object range only to the stripes that intersect it.
pub fn plan_range(
    range: ByteRange,
    object_size: u64,
    stripe_data_bytes: u64,
) -> Result<(ResolvedByteRange, Vec<StripeRange>), ErasureError> {
    if stripe_data_bytes == 0 {
        return Err(ErasureError::InvalidInput(
            "stripe data size must be non-zero".into(),
        ));
    }
    let resolved = range
        .resolve(object_size)
        .map_err(|error| ErasureError::InvalidInput(error.to_string()))?;
    let end = resolved
        .offset
        .checked_add(resolved.length)
        .ok_or_else(|| ErasureError::InvalidInput("range end overflow".into()))?;
    let first = resolved.offset / stripe_data_bytes;
    let last = (end - 1) / stripe_data_bytes;
    let mut stripes = Vec::with_capacity(usize::try_from(last - first + 1).unwrap_or(0));
    for ordinal in first..=last {
        let stripe_start = ordinal.saturating_mul(stripe_data_bytes);
        let intersection_start = resolved.offset.max(stripe_start);
        let intersection_end = end.min(stripe_start.saturating_add(stripe_data_bytes));
        stripes.push(StripeRange {
            stripe_ordinal: ordinal,
            offset: intersection_start - stripe_start,
            length: intersection_end - intersection_start,
        });
    }
    Ok((resolved, stripes))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use proptest::prelude::*;

    use super::*;
    use record_store_storage::upload_stream;

    fn profile(k: u8, m: u8) -> ErasureProfile {
        ErasureProfile::new(k, m).expect("valid profile")
    }

    fn available(stripe: &EncodedStripe) -> Vec<AvailableShard> {
        stripe
            .shards
            .iter()
            .map(|shard| AvailableShard {
                id: shard.manifest.id,
                stripe_id: shard.manifest.stripe_id,
                index: shard.manifest.index,
                bytes: shard.bytes.clone(),
            })
            .collect()
    }

    #[tokio::test]
    async fn round_trip_empty_small_and_partial_stripes() {
        let engine = ErasureEngine::with_codec(Arc::new(ReedSolomonSimdCodec), 2, 1024);
        for payload in [Vec::new(), vec![7], vec![9; 777], vec![3; 1025]] {
            let input = payload.clone();
            let mut encoded = Vec::new();
            let result = engine
                .encode_stream(
                    profile(4, 2),
                    upload_stream(futures_util::stream::iter([Ok(Bytes::from(input))])),
                    |stripe| {
                        encoded.push(stripe);
                        async { Ok(()) }
                    },
                )
                .await
                .expect("encode");
            assert_eq!(result.size, payload.len() as u64);
            let mut decoded = Vec::new();
            for stripe in encoded {
                decoded.extend(
                    engine
                        .decode_one(stripe.manifest.clone(), available(&stripe))
                        .await
                        .expect("decode")
                        .bytes,
                );
            }
            assert_eq!(decoded, payload);
        }
    }

    #[tokio::test]
    async fn reconstructs_data_and_parity_loss_up_to_m() {
        let engine = ErasureEngine::with_codec(Arc::new(ReedSolomonSimdCodec), 1, 4096);
        let stripe = engine
            .encode_one(
                profile(4, 3),
                0,
                0,
                (0_u8..=250).cycle().take(3000).collect(),
            )
            .await
            .expect("encode");
        let original = engine
            .decode_one(stripe.manifest.clone(), available(&stripe))
            .await
            .expect("healthy")
            .bytes;
        for removed in [[0_usize, 4, 6], [1, 2, 5]] {
            let inputs = available(&stripe)
                .into_iter()
                .enumerate()
                .filter_map(|(index, shard)| (!removed.contains(&index)).then_some(shard))
                .collect();
            let decoded = engine
                .decode_one(stripe.manifest.clone(), inputs)
                .await
                .expect("reconstruct");
            assert_eq!(decoded.bytes, original);
            assert!(decoded.reconstructed);
        }
    }

    #[tokio::test]
    async fn refuses_more_than_m_missing_shards() {
        let engine = ErasureEngine::new(1);
        let stripe = engine
            .encode_one(profile(3, 2), 0, 0, vec![11; 4096])
            .await
            .expect("encode");
        let inputs = available(&stripe).into_iter().take(2).collect();
        assert!(matches!(
            engine.decode_one(stripe.manifest, inputs).await,
            Err(ErasureError::Unrecoverable {
                required: 3,
                available: 2
            })
        ));
    }

    #[tokio::test]
    async fn rejects_corruption_but_uses_healthy_fallback_shards() {
        let engine = ErasureEngine::new(1);
        let stripe = engine
            .encode_one(profile(3, 2), 0, 0, vec![22; 3000])
            .await
            .expect("encode");
        let mut inputs = available(&stripe);
        inputs[0].bytes[0] ^= 0xff;
        let decoded = engine
            .decode_one(stripe.manifest.clone(), inputs)
            .await
            .expect("fallback parity reconstructs corrupt data");
        assert_eq!(decoded.bytes, vec![22; 3000]);
        assert_eq!(decoded.corrupt, vec![ShardIndex::new(0).expect("index")]);

        let mut too_corrupt = available(&stripe);
        for shard in too_corrupt.iter_mut().take(3) {
            shard.bytes[0] ^= 0xff;
        }
        assert!(matches!(
            engine.decode_one(stripe.manifest, too_corrupt).await,
            Err(ErasureError::Unrecoverable { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_wrong_ids_stripes_and_duplicates() {
        let engine = ErasureEngine::new(1);
        let stripe = engine
            .encode_one(profile(2, 1), 0, 0, b"identity".to_vec())
            .await
            .expect("encode");
        let mut wrong_id = available(&stripe);
        wrong_id[0].id = ShardId::new();
        assert!(matches!(
            engine.decode_one(stripe.manifest.clone(), wrong_id).await,
            Err(ErasureError::InvalidShardIdentity(_))
        ));
        let mut wrong_stripe = available(&stripe);
        wrong_stripe[0].stripe_id = StripeId::new();
        assert!(matches!(
            engine
                .decode_one(stripe.manifest.clone(), wrong_stripe)
                .await,
            Err(ErasureError::InvalidShardIdentity(_))
        ));
        let mut duplicate = available(&stripe);
        duplicate.push(duplicate[0].clone());
        assert!(matches!(
            engine.decode_one(stripe.manifest, duplicate).await,
            Err(ErasureError::DuplicateShard(0))
        ));
    }

    #[test]
    fn range_planning_targets_only_intersecting_stripes() {
        let (_, within) =
            plan_range(ByteRange::new(3, 4).expect("range"), 30, 10).expect("within stripe");
        assert_eq!(
            within,
            vec![StripeRange {
                stripe_ordinal: 0,
                offset: 3,
                length: 4
            }]
        );
        let (_, crossing) =
            plan_range(ByteRange::new(8, 15).expect("range"), 30, 10).expect("crossing");
        assert_eq!(crossing.len(), 3);
        assert_eq!(crossing[0].length, 2);
        assert_eq!(crossing[2].length, 3);
        let (resolved, end) =
            plan_range(ByteRange::new(25, 50).expect("range"), 30, 10).expect("end range");
        assert_eq!(resolved.length, 5);
        assert_eq!(end.len(), 1);
        assert!(plan_range(ByteRange::new(30, 1).expect("range"), 30, 10).is_err());
    }

    proptest! {
        #[test]
        fn randomized_payload_and_missing_shards_round_trip(
            payload in prop::collection::vec(any::<u8>(), 0..32_768),
            k in 1_u8..=8,
            m in 1_u8..=4,
            missing_seed in any::<u64>(),
        ) {
            let profile = profile(k, m);
            let stripe = encode_stripe(&ReedSolomonSimdCodec, profile, 0, 0, payload.clone())
                .expect("encode");
            let mut inputs = available(&stripe);
            let missing_count = usize::from(m).min(inputs.len());
            let mut indices: Vec<usize> = (0..inputs.len()).collect();
            indices.sort_by_key(|index| {
                missing_seed
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    .rotate_left(u32::try_from(*index).unwrap_or(0))
            });
            let removed: BTreeSet<_> = indices.into_iter().take(missing_count).collect();
            inputs = inputs
                .into_iter()
                .enumerate()
                .filter_map(|(index, shard)| (!removed.contains(&index)).then_some(shard))
                .collect();
            let decoded = decode_stripe(&ReedSolomonSimdCodec, &stripe.manifest, inputs)
                .expect("decode");
            prop_assert_eq!(decoded.bytes, payload);
        }
    }
}
