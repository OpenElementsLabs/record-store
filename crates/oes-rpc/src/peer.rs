//! Peer identity, protocol negotiation, and node authentication.
//!
//! Every internal call carries the caller's protocol version, cluster identity,
//! node identity, and node credential. All four are checked before any cluster
//! state is read or written: private networking alone is not treated as
//! authentication.

use std::sync::Arc;

use async_trait::async_trait;
use oes_cluster::{
    CompatibilityError, NodeVersions, ProtocolVersion, check_compatibility, parse_node_credential,
};
use oes_consensus::ClusterStore;
use oes_core::{ClusterId, NodeId};
use thiserror::Error;
use tonic::{Status, metadata::MetadataMap};

use crate::trace::TraceContext;

/// Header carrying the caller's internal protocol major version.
pub const PROTOCOL_MAJOR_HEADER: &str = "oes-protocol-major";
/// Header carrying the caller's internal protocol minor version.
pub const PROTOCOL_MINOR_HEADER: &str = "oes-protocol-minor";
/// Header carrying the caller's build version.
pub const SOFTWARE_VERSION_HEADER: &str = "oes-software-version";
/// Header carrying the caller's durable replica layout version.
pub const STORAGE_FORMAT_HEADER: &str = "oes-storage-format";
/// Header carrying the caller's durable cluster catalog layout version.
pub const CLUSTER_FORMAT_HEADER: &str = "oes-cluster-format";
/// Header carrying the caller's cluster identity.
pub const CLUSTER_ID_HEADER: &str = "oes-cluster-id";
/// Header carrying the caller's opaque node identity.
pub const NODE_ID_HEADER: &str = "oes-node-id";
/// Header carrying the caller's node credential.
pub const NODE_CREDENTIAL_HEADER: &str = "oes-node-credential";
/// Header carrying a single-use cluster join token.
pub const JOIN_TOKEN_HEADER: &str = "oes-join-token";

/// Failures raised while validating an internal caller.
#[derive(Debug, Error)]
pub enum PeerError {
    /// A required header was missing or malformed.
    #[error("internal request is missing or has a malformed '{0}' header")]
    Header(&'static str),
    /// The caller speaks an incompatible protocol or durable format.
    #[error(transparent)]
    Incompatible(#[from] CompatibilityError),
    /// The caller belongs to a different cluster.
    #[error(
        "caller belongs to cluster {presented} but this node belongs to cluster {local}; \
         reset the joining node or point it at the correct cluster"
    )]
    ClusterMismatch {
        /// Cluster the caller presented.
        presented: ClusterId,
        /// Cluster this node belongs to.
        local: ClusterId,
    },
    /// The node credential was missing, unknown, or disabled.
    #[error("internal request was not authenticated: {0}")]
    Unauthenticated(String),
    /// Cluster state could not be consulted.
    #[error("cluster state could not be consulted: {0}")]
    Unavailable(String),
}

impl From<PeerError> for Status {
    fn from(error: PeerError) -> Self {
        match &error {
            PeerError::Header(_) => Self::invalid_argument(error.to_string()),
            PeerError::Incompatible(_) | PeerError::ClusterMismatch { .. } => {
                Self::failed_precondition(error.to_string())
            }
            PeerError::Unauthenticated(_) => Self::unauthenticated(error.to_string()),
            PeerError::Unavailable(_) => Self::unavailable(error.to_string()),
        }
    }
}

/// A validated internal caller.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    /// Opaque node identity the caller presented.
    pub node_id: NodeId,
    /// Versions the caller advertised.
    pub versions: NodeVersions,
    /// Cluster the caller claims to belong to.
    pub cluster_id: Option<ClusterId>,
    /// Whether a node credential was verified.
    pub authenticated: bool,
    /// Propagated trace context.
    pub trace: TraceContext,
}

/// Verifies node credentials against replicated cluster state.
#[async_trait]
pub trait PeerAuthenticator: Send + Sync {
    /// Returns the cluster this node belongs to, if it has joined one.
    async fn cluster_id(&self) -> Result<Option<ClusterId>, PeerError>;

    /// Verifies a presented node credential.
    async fn verify_credential(&self, presented: &str) -> Result<NodeId, PeerError>;
}

/// Authenticates peers against the replicated cluster catalog.
pub struct CatalogPeerAuthenticator {
    cluster: Arc<dyn ClusterStore>,
}

impl CatalogPeerAuthenticator {
    /// Creates an authenticator over replicated cluster state.
    #[must_use]
    pub const fn new(cluster: Arc<dyn ClusterStore>) -> Self {
        Self { cluster }
    }
}

#[async_trait]
impl PeerAuthenticator for CatalogPeerAuthenticator {
    async fn cluster_id(&self) -> Result<Option<ClusterId>, PeerError> {
        self.cluster
            .identity()
            .await
            .map(|identity| identity.map(|identity| identity.cluster_id))
            .map_err(|error| PeerError::Unavailable(error.to_string()))
    }

    async fn verify_credential(&self, presented: &str) -> Result<NodeId, PeerError> {
        let credential_id = parse_node_credential(presented)
            .map_err(|error| PeerError::Unauthenticated(error.to_string()))?;
        let credential = self
            .cluster
            .node_credential_by_id(credential_id)
            .await
            .map_err(|error| PeerError::Unavailable(error.to_string()))?
            .ok_or_else(|| {
                PeerError::Unauthenticated("node credential is not recognized".into())
            })?;
        credential
            .verify(presented)
            .map_err(|error| PeerError::Unauthenticated(error.to_string()))?;
        Ok(credential.node_id)
    }
}

/// Whether a call requires a verified node credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationRequirement {
    /// The caller must present a valid node credential.
    Required,
    /// The caller may be unauthenticated; the response must stay minimal.
    ///
    /// Only bootstrap and compatibility probes use this, because a node that has
    /// not joined yet has no credential to present.
    Optional,
}

/// Validates internal callers for one node.
pub struct PeerVerifier {
    local: NodeVersions,
    authenticator: Arc<dyn PeerAuthenticator>,
}

impl PeerVerifier {
    /// Creates a verifier for this node's versions.
    #[must_use]
    pub fn new(local: NodeVersions, authenticator: Arc<dyn PeerAuthenticator>) -> Self {
        Self {
            local,
            authenticator,
        }
    }

    /// Validates protocol compatibility, cluster identity, and credentials.
    pub async fn verify(
        &self,
        metadata: &MetadataMap,
        requirement: AuthenticationRequirement,
    ) -> Result<PeerIdentity, PeerError> {
        let versions = NodeVersions {
            protocol: ProtocolVersion::new(
                parse_u32(metadata, PROTOCOL_MAJOR_HEADER)?,
                parse_u32(metadata, PROTOCOL_MINOR_HEADER)?,
            ),
            software: header(metadata, SOFTWARE_VERSION_HEADER)?.to_owned(),
            storage_format: parse_u32(metadata, STORAGE_FORMAT_HEADER)?,
            cluster_format: parse_u32(metadata, CLUSTER_FORMAT_HEADER)?,
        };
        check_compatibility(&self.local, &versions)?;

        let node_id = header(metadata, NODE_ID_HEADER)?
            .parse::<NodeId>()
            .map_err(|_| PeerError::Header(NODE_ID_HEADER))?;
        let presented_cluster = match optional_header(metadata, CLUSTER_ID_HEADER) {
            Some(value) if !value.is_empty() => Some(
                value
                    .parse::<ClusterId>()
                    .map_err(|_| PeerError::Header(CLUSTER_ID_HEADER))?,
            ),
            _ => None,
        };
        if let (Some(presented), Some(local)) =
            (presented_cluster, self.authenticator.cluster_id().await?)
            && presented != local
        {
            return Err(PeerError::ClusterMismatch { presented, local });
        }

        let credential = optional_header(metadata, NODE_CREDENTIAL_HEADER);
        let authenticated = match (credential, requirement) {
            (Some(credential), _) => {
                let verified = self.authenticator.verify_credential(credential).await?;
                if verified != node_id {
                    return Err(PeerError::Unauthenticated(
                        "node credential does not belong to the presented node".into(),
                    ));
                }
                true
            }
            (None, AuthenticationRequirement::Required) => {
                return Err(PeerError::Unauthenticated(
                    "a node credential is required for this operation".into(),
                ));
            }
            (None, AuthenticationRequirement::Optional) => false,
        };

        Ok(PeerIdentity {
            node_id,
            versions,
            cluster_id: presented_cluster,
            authenticated,
            trace: TraceContext::from_metadata(metadata),
        })
    }
}

fn header<'a>(metadata: &'a MetadataMap, name: &'static str) -> Result<&'a str, PeerError> {
    metadata
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or(PeerError::Header(name))
}

fn optional_header<'a>(metadata: &'a MetadataMap, name: &'static str) -> Option<&'a str> {
    metadata.get(name).and_then(|value| value.to_str().ok())
}

fn parse_u32(metadata: &MetadataMap, name: &'static str) -> Result<u32, PeerError> {
    header(metadata, name)?
        .parse()
        .map_err(|_| PeerError::Header(name))
}

/// The identity headers this node attaches to outgoing internal calls.
#[derive(Debug, Clone)]
pub struct PeerHeaders {
    /// This node's opaque identity.
    pub node_id: NodeId,
    /// This node's cluster, once it has joined one.
    pub cluster_id: Option<ClusterId>,
    /// This node's advertised versions.
    pub versions: NodeVersions,
    /// This node's credential, once the cluster has issued one.
    pub credential: Option<String>,
}

impl PeerHeaders {
    /// Writes the identity headers and trace context into request metadata.
    pub fn write(&self, metadata: &mut MetadataMap, trace: &TraceContext) {
        insert(
            metadata,
            PROTOCOL_MAJOR_HEADER,
            &self.versions.protocol.major.to_string(),
        );
        insert(
            metadata,
            PROTOCOL_MINOR_HEADER,
            &self.versions.protocol.minor.to_string(),
        );
        insert(metadata, SOFTWARE_VERSION_HEADER, &self.versions.software);
        insert(
            metadata,
            STORAGE_FORMAT_HEADER,
            &self.versions.storage_format.to_string(),
        );
        insert(
            metadata,
            CLUSTER_FORMAT_HEADER,
            &self.versions.cluster_format.to_string(),
        );
        insert(metadata, NODE_ID_HEADER, &self.node_id.to_string());
        if let Some(cluster_id) = self.cluster_id {
            insert(metadata, CLUSTER_ID_HEADER, &cluster_id.to_string());
        }
        if let Some(credential) = &self.credential {
            insert(metadata, NODE_CREDENTIAL_HEADER, credential);
        }
        trace.write(metadata);
    }
}

fn insert(metadata: &mut MetadataMap, name: &'static str, value: &str) {
    if let Ok(value) = tonic::metadata::MetadataValue::try_from(value) {
        metadata.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysUnknown {
        cluster: Option<ClusterId>,
    }

    #[async_trait]
    impl PeerAuthenticator for AlwaysUnknown {
        async fn cluster_id(&self) -> Result<Option<ClusterId>, PeerError> {
            Ok(self.cluster)
        }

        async fn verify_credential(&self, _presented: &str) -> Result<NodeId, PeerError> {
            Err(PeerError::Unauthenticated("unknown credential".into()))
        }
    }

    fn headers(node_id: NodeId, cluster_id: Option<ClusterId>) -> MetadataMap {
        let mut metadata = MetadataMap::new();
        PeerHeaders {
            node_id,
            cluster_id,
            versions: NodeVersions::current("test"),
            credential: None,
        }
        .write(&mut metadata, &TraceContext::root());
        metadata
    }

    #[tokio::test]
    async fn missing_headers_are_refused() {
        let verifier = PeerVerifier::new(
            NodeVersions::current("test"),
            Arc::new(AlwaysUnknown { cluster: None }),
        );
        let error = verifier
            .verify(&MetadataMap::new(), AuthenticationRequirement::Optional)
            .await
            .expect_err("an unlabelled caller must be refused");
        assert!(matches!(error, PeerError::Header(_)));
    }

    #[tokio::test]
    async fn a_foreign_cluster_is_refused() {
        let local = ClusterId::new();
        let verifier = PeerVerifier::new(
            NodeVersions::current("test"),
            Arc::new(AlwaysUnknown {
                cluster: Some(local),
            }),
        );
        let metadata = headers(NodeId::new(), Some(ClusterId::new()));
        let error = verifier
            .verify(&metadata, AuthenticationRequirement::Optional)
            .await
            .expect_err("a foreign cluster must be refused");
        assert!(matches!(error, PeerError::ClusterMismatch { .. }));
    }

    #[tokio::test]
    async fn an_incompatible_protocol_is_refused() {
        let verifier = PeerVerifier::new(
            NodeVersions {
                protocol: ProtocolVersion::new(2, 0),
                software: "test".into(),
                storage_format: 1,
                cluster_format: 1,
            },
            Arc::new(AlwaysUnknown { cluster: None }),
        );
        let metadata = headers(NodeId::new(), None);
        let error = verifier
            .verify(&metadata, AuthenticationRequirement::Optional)
            .await
            .expect_err("an incompatible peer must be refused");
        assert!(matches!(error, PeerError::Incompatible(_)));
    }

    #[tokio::test]
    async fn a_required_credential_is_enforced() {
        let verifier = PeerVerifier::new(
            NodeVersions::current("test"),
            Arc::new(AlwaysUnknown { cluster: None }),
        );
        let metadata = headers(NodeId::new(), None);
        let error = verifier
            .verify(&metadata, AuthenticationRequirement::Required)
            .await
            .expect_err("an unauthenticated caller must be refused");
        assert!(matches!(error, PeerError::Unauthenticated(_)));
        assert_eq!(Status::from(error).code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn a_compatible_unauthenticated_probe_is_accepted() {
        let verifier = PeerVerifier::new(
            NodeVersions::current("test"),
            Arc::new(AlwaysUnknown { cluster: None }),
        );
        let node_id = NodeId::new();
        let metadata = headers(node_id, None);
        let peer = verifier
            .verify(&metadata, AuthenticationRequirement::Optional)
            .await
            .expect("a compatible probe must be accepted");
        assert_eq!(peer.node_id, node_id);
        assert!(!peer.authenticated);
    }
}
