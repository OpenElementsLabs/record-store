//! Streaming object storage boundary and local filesystem implementation.

use std::{io, sync::Arc};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use md5::Md5;
use record_store_core::{Checksum, ETag, ObjectId, PayloadFormat, ResolvedByteRange};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
};
use zeroize::Zeroizing;

use crate::layout::{
    AES_GCM_TAG_LEN, ENCRYPTED_PAYLOAD_CHUNK_SIZE, ENCRYPTED_PAYLOAD_HEADER_LEN,
    ENCRYPTED_PAYLOAD_MAGIC, OBJECT_ENCRYPTION_ALGORITHM, OBJECT_ENCRYPTION_FORMAT_VERSION,
    ObjectEncryption, ObjectEncryptionRecord, StorageLayout,
};
use crate::maintenance::filesystem;
use crate::maintenance::sync_directory;
use crate::*;

pub(crate) struct WrittenPayload {
    pub(crate) size: u64,
    pub(crate) checksum: Checksum,
    pub(crate) etag: ETag,
    pub(crate) payload_format: PayloadFormat,
}

pub(crate) async fn write_plaintext_payload(
    file: &mut File,
    body: &mut UploadStream,
) -> Result<WrittenPayload, StorageError> {
    let mut strong = Sha256::new();
    let mut md5 = Md5::new();
    let mut size = 0_u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(StorageError::UploadStream)?;
        size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
            filesystem("count upload bytes", io::Error::other("object exceeds u64"))
        })?;
        strong.update(&chunk);
        md5.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|source| filesystem("write upload", source))?;
    }
    Ok(WrittenPayload {
        size,
        checksum: Checksum::sha256(strong.finalize().into()),
        etag: ETag::from_md5(md5.finalize().into()),
        payload_format: PayloadFormat::Plaintext,
    })
}

pub(crate) async fn write_encrypted_payload(
    file: &mut File,
    object_id: ObjectId,
    body: &mut UploadStream,
    encryption: &ObjectEncryption,
) -> Result<WrittenPayload, StorageError> {
    file.write_all(&[0_u8; ENCRYPTED_PAYLOAD_HEADER_LEN])
        .await
        .map_err(|source| filesystem("write encrypted payload header", source))?;
    let data_key = Zeroizing::new(random_array_32());
    let content_nonce = random_array_8();
    let mut strong = Sha256::new();
    let mut md5 = Md5::new();
    let mut size = 0_u64;
    let mut chunk_index = 0_u32;
    let mut pending = Vec::with_capacity(ENCRYPTED_PAYLOAD_CHUNK_SIZE);

    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(StorageError::UploadStream)?;
        size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
            filesystem("count upload bytes", io::Error::other("object exceeds u64"))
        })?;
        strong.update(&chunk);
        md5.update(&chunk);
        let mut remaining = chunk.as_ref();
        while !remaining.is_empty() {
            let take = (ENCRYPTED_PAYLOAD_CHUNK_SIZE - pending.len()).min(remaining.len());
            pending.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if pending.len() == ENCRYPTED_PAYLOAD_CHUNK_SIZE {
                write_encrypted_chunk(
                    file,
                    &data_key,
                    object_id,
                    &content_nonce,
                    chunk_index,
                    &pending,
                )
                .await?;
                pending.clear();
                chunk_index = chunk_index
                    .checked_add(1)
                    .ok_or(StorageError::Cryptography)?;
            }
        }
    }
    if !pending.is_empty() || size == 0 {
        write_encrypted_chunk(
            file,
            &data_key,
            object_id,
            &content_nonce,
            chunk_index,
            &pending,
        )
        .await?;
    }

    let header = encode_encrypted_header(encryption, object_id, size, &data_key, content_nonce)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|source| filesystem("seek encrypted payload header", source))?;
    file.write_all(&header)
        .await
        .map_err(|source| filesystem("finalize encrypted payload header", source))?;
    file.seek(SeekFrom::End(0))
        .await
        .map_err(|source| filesystem("finalize encrypted payload", source))?;

    Ok(WrittenPayload {
        size,
        checksum: Checksum::sha256(strong.finalize().into()),
        etag: ETag::from_md5(md5.finalize().into()),
        payload_format: PayloadFormat::Aes256GcmEnvelopeV1,
    })
}

pub(crate) async fn write_encrypted_chunk(
    file: &mut File,
    data_key: &[u8; 32],
    object_id: ObjectId,
    content_nonce: &[u8; 8],
    index: u32,
    plaintext: &[u8],
) -> Result<(), StorageError> {
    let cipher = Aes256Gcm::new_from_slice(data_key).map_err(|_| StorageError::Cryptography)?;
    let nonce = content_chunk_nonce(content_nonce, index);
    let aad = content_chunk_aad(object_id, index, plaintext.len());
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| StorageError::Cryptography)?;
    file.write_all(&ciphertext)
        .await
        .map_err(|source| filesystem("write encrypted payload chunk", source))
}

pub(crate) fn encode_encrypted_header(
    encryption: &ObjectEncryption,
    object_id: ObjectId,
    size: u64,
    data_key: &[u8; 32],
    content_nonce: [u8; 8],
) -> Result<[u8; ENCRYPTED_PAYLOAD_HEADER_LEN], StorageError> {
    let mut header = [0_u8; ENCRYPTED_PAYLOAD_HEADER_LEN];
    header[..8].copy_from_slice(ENCRYPTED_PAYLOAD_MAGIC);
    header[8..10].copy_from_slice(&(OBJECT_ENCRYPTION_FORMAT_VERSION as u16).to_be_bytes());
    header[10] = 1;
    header[12..16].copy_from_slice(&(ENCRYPTED_PAYLOAD_CHUNK_SIZE as u32).to_be_bytes());
    header[16..24].copy_from_slice(&size.to_be_bytes());
    header[24..40].copy_from_slice(object_id.as_uuid().as_bytes());
    header[40..56].copy_from_slice(&encryption.key_reference);
    let wrap_nonce = random_array_12();
    header[56..68].copy_from_slice(&wrap_nonce);
    header[68..76].copy_from_slice(&content_nonce);
    let cipher = Aes256Gcm::new_from_slice(&encryption.key_encryption_key[..])
        .map_err(|_| StorageError::Cryptography)?;
    let wrapped_key = cipher
        .encrypt(
            Nonce::from_slice(&wrap_nonce),
            Payload {
                msg: data_key,
                aad: &header[..76],
            },
        )
        .map_err(|_| StorageError::Cryptography)?;
    if wrapped_key.len() != 48 {
        return Err(StorageError::Cryptography);
    }
    header[76..].copy_from_slice(&wrapped_key);
    Ok(header)
}

pub(crate) struct EncryptedReadState {
    pub(crate) file: File,
    pub(crate) data_key: Zeroizing<[u8; 32]>,
    pub(crate) object_id: ObjectId,
    pub(crate) content_nonce: [u8; 8],
    pub(crate) plaintext_size: u64,
    pub(crate) next_index: u32,
    pub(crate) end_index: u32,
    pub(crate) first_index: u32,
    pub(crate) first_skip: usize,
    pub(crate) output_remaining: u64,
}

pub(crate) async fn open_encrypted_payload(
    mut file: File,
    object_id: ObjectId,
    size: u64,
    range: Option<ResolvedByteRange>,
    encryption: &ObjectEncryption,
) -> Result<DownloadStream, StorageError> {
    let mut header = [0_u8; ENCRYPTED_PAYLOAD_HEADER_LEN];
    file.read_exact(&mut header)
        .await
        .map_err(|_| StorageError::InconsistentState)?;
    let (data_key, content_nonce) = decode_encrypted_header(&header, encryption, object_id, size)?;
    let chunk_count = encrypted_chunk_count(size)?;
    let expected_file_size = (ENCRYPTED_PAYLOAD_HEADER_LEN as u64)
        .checked_add(size)
        .and_then(|value| value.checked_add(u64::from(chunk_count) * AES_GCM_TAG_LEN as u64))
        .ok_or(StorageError::InconsistentState)?;
    let actual_file_size = file
        .metadata()
        .await
        .map_err(|source| filesystem("inspect encrypted payload", source))?
        .len();
    if actual_file_size != expected_file_size {
        return Err(StorageError::InconsistentState);
    }
    let (offset, length) = range.map_or((0, size), |range| (range.offset, range.length));
    let first_index = u32::try_from(offset / ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64)
        .map_err(|_| StorageError::InconsistentState)?;
    let end_index = if length == 0 {
        first_index
    } else {
        u32::try_from((offset + length - 1) / ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64)
            .map_err(|_| StorageError::InconsistentState)?
    };
    let encrypted_chunk_span = (ENCRYPTED_PAYLOAD_CHUNK_SIZE + AES_GCM_TAG_LEN) as u64;
    let physical_offset = (ENCRYPTED_PAYLOAD_HEADER_LEN as u64)
        .checked_add(u64::from(first_index) * encrypted_chunk_span)
        .ok_or(StorageError::InconsistentState)?;
    file.seek(SeekFrom::Start(physical_offset))
        .await
        .map_err(|source| filesystem("seek encrypted payload", source))?;
    let state = EncryptedReadState {
        file,
        data_key,
        object_id,
        content_nonce,
        plaintext_size: size,
        next_index: first_index,
        end_index,
        first_index,
        first_skip: (offset % ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64) as usize,
        output_remaining: length,
    };
    Ok(Box::pin(stream::try_unfold(
        state,
        |mut state| async move {
            if state.next_index > state.end_index {
                return Ok(None);
            }
            let index = state.next_index;
            let chunk_offset = u64::from(index) * ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64;
            let plaintext_len = if state.plaintext_size == 0 {
                0
            } else {
                usize::try_from(
                    (state.plaintext_size - chunk_offset).min(ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64),
                )
                .map_err(|_| StorageError::InconsistentState)?
            };
            let mut ciphertext = vec![0_u8; plaintext_len + AES_GCM_TAG_LEN];
            state
                .file
                .read_exact(&mut ciphertext)
                .await
                .map_err(|_| StorageError::InconsistentState)?;
            let cipher = Aes256Gcm::new_from_slice(&state.data_key[..])
                .map_err(|_| StorageError::Cryptography)?;
            let nonce = content_chunk_nonce(&state.content_nonce, index);
            let aad = content_chunk_aad(state.object_id, index, plaintext_len);
            let plaintext = cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| StorageError::IntegrityMismatch)?;
            let skip = if index == state.first_index {
                state.first_skip
            } else {
                0
            };
            let available = plaintext.len().saturating_sub(skip);
            let take = available.min(state.output_remaining as usize);
            let output = Bytes::copy_from_slice(&plaintext[skip..skip + take]);
            state.output_remaining -= take as u64;
            state.next_index = state.next_index.saturating_add(1);
            Ok(Some((output, state)))
        },
    )))
}

pub(crate) fn decode_encrypted_header(
    header: &[u8; ENCRYPTED_PAYLOAD_HEADER_LEN],
    encryption: &ObjectEncryption,
    object_id: ObjectId,
    size: u64,
) -> Result<(Zeroizing<[u8; 32]>, [u8; 8]), StorageError> {
    if &header[..8] != ENCRYPTED_PAYLOAD_MAGIC
        || u16::from_be_bytes([header[8], header[9]]) != OBJECT_ENCRYPTION_FORMAT_VERSION as u16
        || header[10] != 1
        || header[11] != 0
    {
        return Err(StorageError::InconsistentState);
    }
    let chunk_size = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
    let encoded_size = u64::from_be_bytes([
        header[16], header[17], header[18], header[19], header[20], header[21], header[22],
        header[23],
    ]);
    if chunk_size != ENCRYPTED_PAYLOAD_CHUNK_SIZE as u32 || encoded_size != size {
        return Err(StorageError::InconsistentState);
    }
    if &header[24..40] != object_id.as_uuid().as_bytes()
        || header[40..56] != encryption.key_reference
    {
        return Err(StorageError::EncryptionKeyMismatch);
    }
    let mut wrap_nonce = [0_u8; 12];
    wrap_nonce.copy_from_slice(&header[56..68]);
    let mut content_nonce = [0_u8; 8];
    content_nonce.copy_from_slice(&header[68..76]);
    let cipher = Aes256Gcm::new_from_slice(&encryption.key_encryption_key[..])
        .map_err(|_| StorageError::Cryptography)?;
    let unwrapped = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&wrap_nonce),
                Payload {
                    msg: &header[76..],
                    aad: &header[..76],
                },
            )
            .map_err(|_| StorageError::IntegrityMismatch)?,
    );
    let mut data_key = Zeroizing::new([0_u8; 32]);
    if unwrapped.len() != data_key.len() {
        return Err(StorageError::InconsistentState);
    }
    data_key.copy_from_slice(&unwrapped);
    Ok((data_key, content_nonce))
}

pub(crate) fn encrypted_chunk_count(size: u64) -> Result<u32, StorageError> {
    let count = if size == 0 {
        1
    } else {
        size.div_ceil(ENCRYPTED_PAYLOAD_CHUNK_SIZE as u64)
    };
    u32::try_from(count).map_err(|_| StorageError::Cryptography)
}

pub(crate) fn content_chunk_nonce(prefix: &[u8; 8], index: u32) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[..8].copy_from_slice(prefix);
    nonce[8..].copy_from_slice(&index.to_be_bytes());
    nonce
}

pub(crate) fn content_chunk_aad(object_id: ObjectId, index: u32, plaintext_len: usize) -> [u8; 32] {
    let mut aad = [0_u8; 32];
    aad[..8].copy_from_slice(ENCRYPTED_PAYLOAD_MAGIC);
    aad[8..24].copy_from_slice(object_id.as_uuid().as_bytes());
    aad[24..28].copy_from_slice(&index.to_be_bytes());
    aad[28..32].copy_from_slice(&(plaintext_len as u32).to_be_bytes());
    aad
}

pub(crate) fn random_array_32() -> [u8; 32] {
    let mut value = [0_u8; 32];
    OsRng.fill_bytes(&mut value);
    value
}

pub(crate) fn random_array_12() -> [u8; 12] {
    let mut value = [0_u8; 12];
    OsRng.fill_bytes(&mut value);
    value
}

pub(crate) fn random_array_8() -> [u8; 8] {
    let mut value = [0_u8; 8];
    OsRng.fill_bytes(&mut value);
    value
}

pub(crate) async fn initialize_object_encryption(
    layout: &StorageLayout,
    master_key: Option<&[u8]>,
) -> Result<Option<ObjectEncryption>, StorageError> {
    let encryption = master_key.map(derive_object_encryption).transpose()?;
    let path = layout.system.join("object-encryption.json");
    match fs::read(&path).await {
        Ok(encoded) => {
            if encoded.len() > 4_096 {
                return Err(StorageError::InconsistentState);
            }
            let record: ObjectEncryptionRecord = serde_json::from_slice(&encoded)?;
            if record.encryption_format_version != OBJECT_ENCRYPTION_FORMAT_VERSION
                || record.algorithm != OBJECT_ENCRYPTION_ALGORITHM
            {
                return Err(StorageError::InconsistentState);
            }
            let encryption = encryption.ok_or(StorageError::EncryptionKeyRequired)?;
            if record.key_reference != hex::encode(encryption.key_reference) {
                return Err(StorageError::EncryptionKeyMismatch);
            }
            Ok(Some(encryption))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let Some(encryption) = encryption else {
                return Ok(None);
            };
            let encoded = serde_json::to_vec(&ObjectEncryptionRecord {
                encryption_format_version: OBJECT_ENCRYPTION_FORMAT_VERSION,
                algorithm: OBJECT_ENCRYPTION_ALGORITHM.to_owned(),
                key_reference: hex::encode(encryption.key_reference),
            })?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
                .map_err(|source| filesystem("create object encryption format", source))?;
            file.write_all(&encoded)
                .await
                .map_err(|source| filesystem("write object encryption format", source))?;
            file.sync_all()
                .await
                .map_err(|source| filesystem("synchronize object encryption format", source))?;
            sync_directory(layout.system.clone()).await?;
            Ok(Some(encryption))
        }
        Err(source) => Err(filesystem("read object encryption format", source)),
    }
}

pub(crate) fn derive_object_encryption(
    master_key: &[u8],
) -> Result<ObjectEncryption, StorageError> {
    let derivation =
        hkdf::Hkdf::<Sha256>::new(Some(b"record-store-object-encryption-v1"), master_key);
    let mut key = Zeroizing::new([0_u8; 32]);
    derivation
        .expand(b"object-key-encryption-key", &mut *key)
        .map_err(|_| StorageError::Cryptography)?;
    let digest = Sha256::digest(&key[..]);
    let mut key_reference = [0_u8; 16];
    key_reference.copy_from_slice(&digest[..16]);
    Ok(ObjectEncryption {
        key_encryption_key: Arc::new(key),
        key_reference,
    })
}
