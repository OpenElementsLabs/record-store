use crate::*;

pub(crate) fn validate_service_account_name(name: &str) -> Result<(), CredentialStoreError> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(CredentialStoreError::InvalidInput(
            "name must contain 1 to 128 non-control characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_description(description: &str) -> Result<(), CredentialStoreError> {
    if description.len() > 1_024 || description.chars().any(char::is_control) {
        return Err(CredentialStoreError::InvalidInput(
            "description must not exceed 1024 non-control characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_policy_statements(
    statements: &[PolicyStatement],
) -> Result<(), CredentialStoreError> {
    if statements.is_empty() || statements.len() > 128 {
        return Err(CredentialStoreError::InvalidInput(
            "policy must contain between 1 and 128 statements".into(),
        ));
    }
    for statement in statements {
        if statement.actions.is_empty() || statement.resources.is_empty() {
            return Err(CredentialStoreError::InvalidInput(
                "policy statements require actions and resources".into(),
            ));
        }
        for resource in &statement.resources {
            let wildcard_count = resource.bytes().filter(|byte| *byte == b'*').count();
            if !resource.starts_with("bucket:")
                || wildcard_count > 1
                || (wildcard_count == 1 && !resource.ends_with('*'))
                || resource.chars().any(char::is_control)
            {
                return Err(CredentialStoreError::InvalidInput(
                    "policy resources must use bucket:... with an optional final wildcard".into(),
                ));
            }
        }
    }
    Ok(())
}
