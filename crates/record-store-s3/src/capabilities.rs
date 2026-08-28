/// Stable support level for the machine-testable S3 compatibility registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// The operation is implemented and covered by protocol tests.
    Implemented,
    /// A useful subset is implemented with explicit unsupported semantics.
    Partial,
    /// Requests are rejected with `NotImplemented`.
    Unsupported,
}

/// One low-cardinality S3 capability descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S3Capability {
    pub name: &'static str,
    pub status: CapabilityStatus,
}

/// Testable compatibility surface. Keep this synchronized with routing and
/// protocol tests instead of maintaining a separate status document.
pub const S3_CAPABILITIES: &[S3Capability] = &[
    S3Capability {
        name: "SigV4HeaderAuthentication",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "PresignedGetObject",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "PresignedPutObject",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "BucketOperations",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "BucketCors",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ObjectOperations",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ListObjectsV2",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "MultipartUpload",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "UploadPartCopy",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "ObjectVersioning",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "CopyObject",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "RangeAndConditionalReads",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ClientSha256Checksums",
        status: CapabilityStatus::Implemented,
    },
    S3Capability {
        name: "ServerSideEncryptionHeaders",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "AccessControlLists",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "ObjectLock",
        status: CapabilityStatus::Unsupported,
    },
    S3Capability {
        name: "AwsChunkedEncoding",
        status: CapabilityStatus::Unsupported,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_registry_is_unique_and_tracks_explicit_gaps() {
        let names = S3_CAPABILITIES
            .iter()
            .map(|capability| capability.name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), S3_CAPABILITIES.len());
        assert!(S3_CAPABILITIES.iter().any(|capability| {
            capability.name == "MultipartUpload"
                && capability.status == CapabilityStatus::Implemented
        }));
        assert!(S3_CAPABILITIES.iter().any(|capability| {
            capability.name == "UploadPartCopy"
                && capability.status == CapabilityStatus::Unsupported
        }));
    }
}
