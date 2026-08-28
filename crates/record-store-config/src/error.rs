//! Configuration loading, environment overrides, secret redaction, and validation.

use std::{
    collections::HashMap,
    ffi::OsString,
    fmt::{Debug, Display},
    path::PathBuf,
};

use thiserror::Error;

/// Configuration loading and validation failures.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The selected configuration file could not be read.
    #[error("failed to read configuration file '{}': {source}", path.display())]
    ReadFile {
        /// Selected file path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The selected file was not valid Record Store TOML.
    #[error("failed to parse configuration file '{}': {source}", path.display())]
    ParseFile {
        /// Selected file path.
        path: PathBuf,
        /// TOML decoding error.
        #[source]
        source: toml::de::Error,
    },
    /// An environment variable was not valid Unicode.
    #[error("environment variable {0} is not valid Unicode")]
    NonUnicodeEnvironment(&'static str),
    /// An environment value could not be parsed. Its value is intentionally omitted.
    #[error("environment variable {name} is invalid: {reason}")]
    InvalidEnvironment {
        /// Variable name.
        name: &'static str,
        /// Expected type or parser failure.
        reason: String,
    },
    /// One or more resolved settings were invalid.
    #[error("configuration validation failed: {0}")]
    Validation(String),
}

pub(crate) fn environment_value<'a>(
    environment: &'a HashMap<OsString, OsString>,
    name: &'static str,
) -> Result<Option<&'a str>, ConfigError> {
    let Some(value) = environment.get(&OsString::from(name)) else {
        return Ok(None);
    };
    value
        .to_str()
        .map(Some)
        .ok_or(ConfigError::NonUnicodeEnvironment(name))
}

pub(crate) fn parse_environment<T>(name: &'static str, value: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: Display,
{
    value
        .parse()
        .map_err(|error: T::Err| ConfigError::InvalidEnvironment {
            name,
            reason: error.to_string(),
        })
}
