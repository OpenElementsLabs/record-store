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
