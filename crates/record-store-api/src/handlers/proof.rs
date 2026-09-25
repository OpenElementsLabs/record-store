//! Emitting a portable proof bundle for one object version.
//!
//! The bundle is assembled and signed by the server because the server is what
//! holds the deployment's master key; the key never reaches a client. What
//! comes back is an ordinary JSON document that carries no credential and no
//! payload, so it is safe to hand to whoever needs to check the object.

use axum::{
    Json,
    extract::{Extension, Path, Query, State},
};
use record_store_core::{BucketName, ObjectKey, ObjectMetadata, VersionId};
use record_store_proof::{
    BUNDLE_FORMAT, BUNDLE_FORMAT_VERSION, BundleSignature, Deployment, History, HistoryUnavailable,
    ObjectIdentity, PayloadDigest, ProofBundle,
};
use serde::Deserialize;

use crate::error::{ApiError, service_to_api_error};
use crate::*;

#[derive(Deserialize)]
pub(crate) struct ProofQuery {
    #[serde(default)]
    version_id: Option<VersionId>,
}

/// Returns a signed proof bundle for one object version.
pub(crate) async fn object_proof(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    Query(query): Query<ProofQuery>,
    Extension(request_id): Extension<RequestId>,
) -> Result<Json<ProofBundle>, ApiError> {
    let Some(signer) = state.proof_signer.clone() else {
        // An unsigned bundle would look like a signed one to anyone not reading
        // closely, so none is produced at all.
        return Err(ApiError::bad_request(
            request_id,
            "PROOF_SIGNING_UNAVAILABLE",
            "Proof bundles require the deployment master key, which is not configured",
        ));
    };
    let name = BucketName::new(bucket).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_BUCKET_NAME",
            "Invalid bucket name",
        )
    })?;
    let key = ObjectKey::new(key).map_err(|_| {
        ApiError::bad_request(
            request_id.clone(),
            "INVALID_OBJECT_KEY",
            "Invalid object key",
        )
    })?;

    let metadata: ObjectMetadata = match query.version_id {
        Some(version_id) => {
            state
                .services
                .objects
                .head_version(&name, key, version_id)
                .await
        }
        None => state.services.objects.head(&name, key).await,
    }
    .map_err(|error| service_to_api_error(error, request_id))?;

    let mut bundle = ProofBundle {
        format: BUNDLE_FORMAT.to_owned(),
        format_version: BUNDLE_FORMAT_VERSION,
        object: ObjectIdentity {
            bucket: name.to_string(),
            key: metadata.key.to_string(),
            version_id: metadata.version_id.to_string(),
            size: metadata.size,
            content_type: metadata.content_type.clone(),
            created_at: metadata.created_at,
        },
        payload: PayloadDigest {
            sha256: payload_sha256(&metadata),
        },
        // The audit log is chained, but nothing checkpoints it yet, and a
        // bundle's history section exists to carry a root and an inclusion
        // path. Saying so in the document is deliberate: a missing section
        // reads as "nothing happened", where this reads as "this was not
        // established, and here is precisely what is missing".
        history: History::Unavailable {
            reason: HistoryUnavailable::NotYetCheckpointed,
            detail: "this deployment maintains a hash-chained audit log, which detects a \
                     record edited or removed by anyone who cannot rewrite every later \
                     link. It does not yet produce checkpoints or external anchors, so \
                     no Merkle root and no inclusion path can be included here, and this \
                     bundle establishes nothing against an operator who rewrote the whole \
                     log. Verify the chain directly with GET /api/v1/audit/chain."
                .to_owned(),
        },
        deployment: Deployment {
            algorithm: String::new(),
            public_key: String::new(),
            key_id: String::new(),
        },
        signature: BundleSignature {
            algorithm: String::new(),
            canonical_version: 0,
            value: String::new(),
        },
    };
    signer.sign(&mut bundle);
    Ok(Json(bundle))
}

/// Returns the payload digest as lowercase hex.
///
/// The stored checksum renders as `sha256:<hex>`; the bundle carries the digest
/// on its own so a reader can paste it straight into `sha256sum`.
fn payload_sha256(metadata: &ObjectMetadata) -> String {
    let rendered = metadata.checksum.to_string();
    rendered
        .split_once(':')
        .map_or(rendered.clone(), |(_, digest)| digest.to_owned())
}
