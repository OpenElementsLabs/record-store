use std::{
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::*;

macro_rules! uuid_identifier {
    ($name:ident, $kind:literal) => {
        #[doc = concat!("Strongly typed ", $kind, " identifier.")]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[doc = concat!("Creates a random ", $kind, " identifier.")]
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Creates the typed identifier from an existing UUID.
            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                Display::fmt(&self.0, formatter)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .map(Self)
                    .map_err(|error| CoreError::InvalidIdentifier {
                        kind: $kind,
                        reason: error.to_string(),
                    })
            }
        }
    };
}

uuid_identifier!(BucketId, "bucket");
uuid_identifier!(ObjectId, "object");
uuid_identifier!(VersionId, "version");
uuid_identifier!(UploadId, "multipart upload");
uuid_identifier!(NodeId, "node");
uuid_identifier!(DeviceId, "device");
uuid_identifier!(ClusterId, "cluster");
uuid_identifier!(OrganizationId, "organization");
uuid_identifier!(ServiceAccountId, "service account");
uuid_identifier!(CredentialId, "credential");
uuid_identifier!(PolicyId, "policy");
uuid_identifier!(AuditEventId, "audit event");
uuid_identifier!(EventId, "storage event");
uuid_identifier!(WebhookId, "webhook");
uuid_identifier!(LifecycleRuleId, "lifecycle rule");
uuid_identifier!(ReplicaTaskId, "replica task");
uuid_identifier!(ClusterOperationId, "cluster operation");
uuid_identifier!(JoinTokenId, "join token");
uuid_identifier!(NodeCredentialId, "node credential");
uuid_identifier!(StripeId, "erasure stripe");
uuid_identifier!(ShareLinkId, "share link");
uuid_identifier!(EmbedLinkId, "embed link");
uuid_identifier!(ShardId, "erasure shard");

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn typed_identifiers_are_not_interchangeable() {
        let raw = Uuid::new_v4();
        let bucket = BucketId::from_uuid(raw);
        let object = ObjectId::from_uuid(raw);
        assert_eq!(bucket.to_string(), object.to_string());
        assert_eq!(bucket.as_uuid(), raw);
    }
}
