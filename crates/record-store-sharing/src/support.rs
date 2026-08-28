use std::sync::Arc;

use record_store_core::VersionId;

use crate::*;

/// A shared handle to the sharing service.
pub type SharedSharingService = Arc<SharingService>;

/// Longest operator-facing capability label.
pub const MAXIMUM_LABEL_LENGTH: usize = 120;

pub(crate) fn validated_label(label: &str) -> Result<String, SharingError> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err(SharingError::Invalid(
            "a link needs a name so it can be recognised later".to_owned(),
        ));
    }
    if trimmed.chars().count() > MAXIMUM_LABEL_LENGTH {
        return Err(SharingError::Invalid(format!(
            "a link name must be at most {MAXIMUM_LABEL_LENGTH} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(SharingError::Invalid(
            "a link name must not contain control characters".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Resolves the version a capability points at, for a caller that will then read
/// it through the authoritative object service.
#[must_use]
pub const fn pinned_version(target: &CapabilityTarget) -> Option<VersionId> {
    target.version.pinned()
}

#[cfg(test)]
mod tests {

    use chrono::Utc;
    use record_store_core::VersionId;

    use super::*;
    use crate::test_support::*;

    #[tokio::test]
    async fn labels_are_validated_rather_than_stored_as_typed() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        for label in ["", "   ", "with\u{0}control"] {
            let mut request = share_request();
            request.label = label.to_owned();
            assert!(
                service.create_share(request, now).await.is_err(),
                "accepted label {label:?}"
            );
        }
        let mut request = share_request();
        request.label = "  Board review  ".to_owned();
        let issued = service.create_share(request, now).await.expect("create");
        assert_eq!(issued.link.label, "Board review");
    }

    #[tokio::test]
    async fn a_pinned_share_records_the_exact_version_it_was_created_for() {
        let service = service(SharingPolicy::default()).await;
        let now = Utc::now();
        let version = VersionId::new();
        let mut request = share_request();
        request.version = VersionMode::Pinned {
            version_id: version,
        };
        let issued = service.create_share(request, now).await.expect("create");
        assert_eq!(pinned_version(&issued.link.target), Some(version));

        let resolved = service
            .store()
            .resolve_share(issued.token.digest())
            .await
            .expect("resolve")
            .expect("share");
        assert_eq!(resolved.target.version.pinned(), Some(version));
    }
}
