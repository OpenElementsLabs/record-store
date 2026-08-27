//! Process-wide structured tracing initialization.

use record_store_config::ObservabilityConfig;
use thiserror::Error;
use tracing_subscriber::{EnvFilter, fmt};

/// Initializes the process tracing subscriber exactly once.
///
/// The subscriber is assembled from layers so an OpenTelemetry layer can be
/// added later without changing callers or domain crates.
pub fn init(config: &ObservabilityConfig) -> Result<(), ObservabilityError> {
    let filter = EnvFilter::try_new(&config.log_filter)
        .map_err(|error| ObservabilityError::InvalidFilter(error.to_string()))?;

    if config.json {
        fmt()
            .with_env_filter(filter)
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .try_init()
            .map_err(|error| ObservabilityError::Install(error.to_string()))
    } else {
        fmt()
            .with_env_filter(filter)
            .compact()
            .try_init()
            .map_err(|error| ObservabilityError::Install(error.to_string()))
    }
}

/// Failures while configuring process observability.
#[derive(Debug, Error)]
pub enum ObservabilityError {
    /// The configured logging directive is not valid.
    #[error("invalid log filter: {0}")]
    InvalidFilter(String),
    /// A global subscriber was already installed or could not be registered.
    #[error("failed to install tracing subscriber: {0}")]
    Install(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_filter_is_rejected_before_installation() {
        let config = ObservabilityConfig {
            log_filter: "not a valid[filter".into(),
            json: false,
        };
        assert!(matches!(
            init(&config),
            Err(ObservabilityError::InvalidFilter(_))
        ));
    }
}
