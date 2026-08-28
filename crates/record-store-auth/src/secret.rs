use std::fmt::{self, Debug, Formatter};

use zeroize::Zeroizing;

/// Secret signing material with zeroization and a redacted debug representation.
pub struct SigningSecret(Zeroizing<Vec<u8>>);

impl SigningSecret {
    /// Copies secret bytes into zeroizing memory.
    #[must_use]
    pub fn new(value: impl AsRef<[u8]>) -> Self {
        Self(Zeroizing::new(value.as_ref().to_vec()))
    }

    /// Exposes signing bytes only to cryptographic code.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Debug for SigningSecret {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted signing secret>")
    }
}
