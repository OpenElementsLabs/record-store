//! Transport security for internal RPC.
//!
//! Internal traffic carries object bytes, credentials, and metadata. Plaintext
//! is supported for single-host development only; a production deployment is
//! expected to configure TLS, and mutual TLS when node certificates are issued.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

/// Failures raised while preparing transport security.
#[derive(Debug, Error)]
pub enum TlsError {
    /// A configured key or certificate file could not be read.
    #[error("could not read internal TLS material from {path}: {source}")]
    Read {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The TLS configuration was incomplete or contradictory.
    #[error("invalid internal TLS configuration: {0}")]
    Configuration(String),
}

/// Node-local transport security settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsSettings {
    /// PEM certificate chain presented by this node.
    pub certificate_path: Option<PathBuf>,
    /// PEM private key for the presented certificate.
    pub private_key_path: Option<PathBuf>,
    /// PEM certificate authority used to verify peer server certificates.
    pub peer_ca_path: Option<PathBuf>,
    /// PEM certificate authority used to require and verify client certificates.
    ///
    /// Setting this turns on mutual authentication at the transport layer, in
    /// addition to the node credential carried on every call.
    pub client_ca_path: Option<PathBuf>,
    /// Server name presented during the handshake, when it differs from the
    /// advertised address.
    pub server_name: Option<String>,
}

impl TlsSettings {
    /// Returns whether transport security is configured at all.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.certificate_path.is_some() || self.peer_ca_path.is_some()
    }

    /// Returns whether mutual TLS is required of peers.
    #[must_use]
    pub const fn mutual(&self) -> bool {
        self.client_ca_path.is_some()
    }

    /// Validates the combination of configured files.
    pub fn validate(&self) -> Result<(), TlsError> {
        if self.certificate_path.is_some() != self.private_key_path.is_some() {
            return Err(TlsError::Configuration(
                "a certificate and a private key must be configured together".into(),
            ));
        }
        if self.client_ca_path.is_some() && self.certificate_path.is_none() {
            return Err(TlsError::Configuration(
                "mutual TLS requires this node to present its own certificate".into(),
            ));
        }
        Ok(())
    }

    /// Builds the server-side configuration, when one is configured.
    pub fn server_config(&self) -> Result<Option<ServerTlsConfig>, TlsError> {
        self.validate()?;
        let (Some(certificate), Some(key)) = (&self.certificate_path, &self.private_key_path)
        else {
            return Ok(None);
        };
        let identity = Identity::from_pem(read(certificate)?, read(key)?);
        let mut config = ServerTlsConfig::new().identity(identity);
        if let Some(client_ca) = &self.client_ca_path {
            config = config.client_ca_root(Certificate::from_pem(read(client_ca)?));
        }
        Ok(Some(config))
    }

    /// Builds the client-side configuration, when one is configured.
    pub fn client_config(&self) -> Result<Option<ClientTlsConfig>, TlsError> {
        self.validate()?;
        if !self.enabled() {
            return Ok(None);
        }
        let mut config = ClientTlsConfig::new();
        if let Some(peer_ca) = &self.peer_ca_path {
            config = config.ca_certificate(Certificate::from_pem(read(peer_ca)?));
        }
        if let (Some(certificate), Some(key)) = (&self.certificate_path, &self.private_key_path) {
            config = config.identity(Identity::from_pem(read(certificate)?, read(key)?));
        }
        if let Some(name) = &self.server_name {
            config = config.domain_name(name.clone());
        }
        Ok(Some(config))
    }
}

fn read(path: &Path) -> Result<Vec<u8>, TlsError> {
    std::fs::read(path).map_err(|source| TlsError::Read {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_is_reported_as_disabled() {
        let settings = TlsSettings::default();
        assert!(!settings.enabled());
        assert!(!settings.mutual());
        assert!(settings.server_config().expect("validate").is_none());
        assert!(settings.client_config().expect("validate").is_none());
    }

    #[test]
    fn a_certificate_without_a_key_is_refused() {
        let settings = TlsSettings {
            certificate_path: Some(PathBuf::from("/nonexistent/cert.pem")),
            ..TlsSettings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(TlsError::Configuration(_))
        ));
    }

    #[test]
    fn mutual_tls_requires_a_local_identity() {
        let settings = TlsSettings {
            client_ca_path: Some(PathBuf::from("/nonexistent/ca.pem")),
            ..TlsSettings::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(TlsError::Configuration(_))
        ));
    }

    #[test]
    fn unreadable_material_is_reported_with_its_path() {
        let settings = TlsSettings {
            certificate_path: Some(PathBuf::from("/nonexistent/cert.pem")),
            private_key_path: Some(PathBuf::from("/nonexistent/key.pem")),
            ..TlsSettings::default()
        };
        assert!(matches!(
            settings.server_config(),
            Err(TlsError::Read { .. })
        ));
    }
}
