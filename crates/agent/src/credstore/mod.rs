//! On-disk storage for the agent's enrolled credential (certificate, private
//! key, CA and server details), protected at rest.
//!
//! - Windows: DPAPI ([`dpapi`]), bound to the account the agent runs as
//!   (SYSTEM once installed as a service).
//! - Elsewhere: a dev-only encrypted file ([`devfile`]). See the TODO there.

#[cfg(not(windows))]
mod devfile;
#[cfg(windows)]
mod dpapi;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::Identity;
use serde::{Deserialize, Serialize};

use crate::AgentConfig;

/// File holding the protected credential inside the state directory.
pub const CREDENTIAL_FILE: &str = "credential.bin";

/// Everything an enrolled agent needs to reconnect.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub agent_id: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub ca_pem: String,
    pub server_addr: SocketAddr,
    pub server_name: String,
    pub api_url: String,
}

// Keep the private key out of logs and panic messages.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("agent_id", &self.agent_id)
            .field("server_addr", &self.server_addr)
            .field("server_name", &self.server_name)
            .field("api_url", &self.api_url)
            .finish_non_exhaustive()
    }
}

impl Credential {
    /// Connection settings for the control connection.
    pub fn agent_config(&self, heartbeat_interval: Duration) -> Result<AgentConfig, StoreError> {
        Ok(AgentConfig {
            server_addr: self.server_addr,
            server_name: self.server_name.clone(),
            agent_id: self.agent_id.clone(),
            server_ca: common::tls::certs_from_pem(&self.ca_pem)?,
            identity: Identity::from_pem(&self.cert_pem, &self.key_pem)?,
            heartbeat_interval,
            bind_addr: None,
            telemetry: None,
            media: None,
            desktop: None,
            device_kind: crate::device::kind(),
            hostname: crate::device::hostname(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("credential could not be decrypted (corrupt, tampered with, or from another machine/account)")]
    Decrypt,
    #[error("credential protection failed: {0}")]
    Protect(String),
    #[error("credential is malformed: {0}")]
    Format(#[from] serde_json::Error),
    #[error(transparent)]
    Tls(#[from] common::TlsError),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

/// The credential store in one state directory.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn credential_path(&self) -> PathBuf {
        self.dir.join(CREDENTIAL_FILE)
    }

    pub fn exists(&self) -> bool {
        self.credential_path().exists()
    }

    pub fn save(&self, credential: &Credential) -> Result<(), StoreError> {
        std::fs::create_dir_all(&self.dir).map_err(io_err(&self.dir))?;
        let plaintext = serde_json::to_vec(credential)?;
        let blob = protect(&self.dir, &plaintext)?;
        let path = self.credential_path();
        common::fs::write_file(&path, &blob, true).map_err(io_err(&path))
    }

    pub fn load(&self) -> Result<Credential, StoreError> {
        let path = self.credential_path();
        let blob = std::fs::read(&path).map_err(io_err(&path))?;
        let plaintext = unprotect(&self.dir, &blob)?;
        Ok(serde_json::from_slice(&plaintext)?)
    }
}

#[cfg(windows)]
fn protect(_dir: &Path, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    dpapi::protect(plaintext)
}

#[cfg(windows)]
fn unprotect(_dir: &Path, blob: &[u8]) -> Result<Vec<u8>, StoreError> {
    dpapi::unprotect(blob)
}

#[cfg(not(windows))]
fn protect(dir: &Path, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    devfile::protect(dir, plaintext)
}

#[cfg(not(windows))]
fn unprotect(dir: &Path, blob: &[u8]) -> Result<Vec<u8>, StoreError> {
    devfile::unprotect(dir, blob)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Credential {
        let certs = common::devcerts::generate("agt-test").unwrap();
        Credential {
            agent_id: "agt-test".into(),
            cert_pem: certs.agent_cert.clone(),
            key_pem: certs.agent_key.clone(),
            ca_pem: certs.ca_cert.clone(),
            server_addr: "127.0.0.1:4433".parse().unwrap(),
            server_name: "localhost".into(),
            api_url: "https://localhost:8443".into(),
        }
    }

    #[test]
    fn round_trips_and_builds_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("state"));
        assert!(!store.exists());
        let cred = sample();
        store.save(&cred).unwrap();
        assert!(store.exists());
        let loaded = store.load().unwrap();
        assert_eq!(loaded, cred);
        let config = loaded.agent_config(Duration::from_secs(5)).unwrap();
        assert_eq!(config.agent_id, "agt-test");
    }

    #[test]
    fn stored_file_does_not_contain_the_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        let cred = sample();
        store.save(&cred).unwrap();
        let blob = std::fs::read(dir.path().join(CREDENTIAL_FILE)).unwrap();
        let key_body = cred.key_pem.lines().nth(1).unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains(key_body));
        assert!(!String::from_utf8_lossy(&blob).contains("PRIVATE KEY"));
    }

    #[test]
    fn tampered_credential_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        store.save(&sample()).unwrap();
        let path = dir.path().join(CREDENTIAL_FILE);
        let mut blob = std::fs::read(&path).unwrap();
        let mid = blob.len() / 2;
        blob[mid] ^= 0x01;
        std::fs::write(&path, blob).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Decrypt)));
    }

    #[test]
    fn debug_output_hides_the_key() {
        let cred = sample();
        let shown = format!("{cred:?}");
        assert!(shown.contains("agt-test"));
        assert!(!shown.contains("PRIVATE KEY"));
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path());
        store.save(&sample()).unwrap();
        let mode = std::fs::metadata(dir.path().join(CREDENTIAL_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
