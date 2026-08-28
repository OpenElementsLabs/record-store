use std::{
    fmt::{self, Display, Formatter},
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::*;

/// Supported content-integrity algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    /// SHA-256, stored as a 32-byte digest.
    Sha256,
}

impl Display for ChecksumAlgorithm {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha256 => formatter.write_str("sha256"),
        }
    }
}

/// A content checksum with an explicit algorithm discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Checksum {
    algorithm: ChecksumAlgorithm,
    digest: Vec<u8>,
}

impl Checksum {
    /// Creates a SHA-256 checksum from a complete digest.
    #[must_use]
    pub fn sha256(digest: [u8; 32]) -> Self {
        Self {
            algorithm: ChecksumAlgorithm::Sha256,
            digest: digest.to_vec(),
        }
    }

    /// Returns the checksum algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> ChecksumAlgorithm {
        self.algorithm
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub fn digest(&self) -> &[u8] {
        &self.digest
    }
}

impl Display for Checksum {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}",
            self.algorithm,
            hex::encode(&self.digest)
        )
    }
}

impl FromStr for Checksum {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (algorithm, encoded) = value.split_once(':').ok_or_else(|| {
            CoreError::InvalidChecksum("expected '<algorithm>:<hex digest>'".into())
        })?;
        let digest = hex::decode(encoded)
            .map_err(|error| CoreError::InvalidChecksum(format!("invalid hex digest: {error}")))?;
        match algorithm {
            "sha256" if digest.len() == 32 => Ok(Self {
                algorithm: ChecksumAlgorithm::Sha256,
                digest,
            }),
            "sha256" => Err(CoreError::InvalidChecksum(
                "SHA-256 digest must contain exactly 32 bytes".into(),
            )),
            _ => Err(CoreError::InvalidChecksum(format!(
                "unsupported algorithm '{algorithm}'"
            ))),
        }
    }
}

impl TryFrom<String> for Checksum {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Checksum> for String {
    fn from(value: Checksum) -> Self {
        value.to_string()
    }
}

/// A stable protocol-facing entity tag, stored without HTTP quote characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ETag(String);

impl ETag {
    /// Creates a validated opaque entity tag.
    pub fn new(value: impl Into<String>) -> Result<Self, CoreError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'"')
        {
            return Err(CoreError::InvalidETag(
                "ETag must contain 1 to 128 visible ASCII characters excluding quotes".into(),
            ));
        }
        Ok(Self(value))
    }

    /// Builds the compatibility ETag for a single-part upload.
    #[must_use]
    pub fn from_md5(digest: [u8; 16]) -> Self {
        Self(hex::encode(digest))
    }

    /// Returns the unquoted ETag value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ETag {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for ETag {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ETag> for String {
    fn from(value: ETag) -> Self {
        value.0
    }
}
