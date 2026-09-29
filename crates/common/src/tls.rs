//! Loading PEM certificates and keys.

use std::path::{Path, PathBuf};

use rustls_pki_types::pem::{self, PemObject};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("reading PEM from {path}: {source}")]
    Pem {
        path: PathBuf,
        #[source]
        source: pem::Error,
    },
    #[error("parsing PEM: {0}")]
    PemData(#[from] pem::Error),
    #[error("no certificates found in {0}")]
    NoCertificates(String),
    #[error("tls configuration: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("client certificate verifier: {0}")]
    Verifier(#[from] rustls::server::VerifierBuilderError),
    #[error("tls configuration has no QUIC-compatible cipher suite: {0}")]
    Quic(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("certificate generation: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A certificate chain and its private key: what a peer presents during the handshake.
#[derive(Debug)]
pub struct Identity {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self {
            cert_chain: self.cert_chain.clone(),
            key: self.key.clone_key(),
        }
    }
}

impl Identity {
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, TlsError> {
        Ok(Self {
            cert_chain: certs_from_pem(cert_pem)?,
            key: PrivateKeyDer::from_pem_slice(key_pem.as_bytes())?,
        })
    }

    pub fn from_files(cert: &Path, key: &Path) -> Result<Self, TlsError> {
        Ok(Self {
            cert_chain: load_certs(cert)?,
            key: PrivateKeyDer::from_pem_file(key).map_err(|source| TlsError::Pem {
                path: key.to_owned(),
                source,
            })?,
        })
    }
}

/// Parse every certificate in a PEM string. Errors if there are none.
pub fn certs_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates("PEM input".into()));
    }
    Ok(certs)
}

/// Load every certificate in a PEM file. Errors if there are none.
pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let pem_err = |source| TlsError::Pem {
        path: path.to_owned(),
        source,
    };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(pem_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(pem_err)?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates(path.display().to_string()));
    }
    Ok(certs)
}

/// rustls config for an HTTPS server presenting `identity` (no client certs).
pub fn https_server_config(identity: &Identity) -> Result<rustls::ServerConfig, TlsError> {
    let mut config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(identity.cert_chain.clone(), identity.key.clone_key())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}
