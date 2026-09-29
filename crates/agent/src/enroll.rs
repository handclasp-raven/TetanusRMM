//! First-run enrollment: trade a one-time token for a client certificate.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use protocol::{close_code, read_frame, write_frame, Message};
use rcgen::{CertificateParams, KeyPair};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::credstore::Credential;
use crate::AgentError;

pub struct EnrollOptions {
    pub server_addr: SocketAddr,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    /// PEM CA that the server certificate must chain to.
    pub server_ca_pem: String,
    pub token: String,
    pub bind_addr: Option<SocketAddr>,
}

/// Enroll with the server and return the credential to store.
///
/// The key pair is generated here and only a CSR (public key plus a
/// signature proving possession) is sent, so the private key never leaves
/// this machine.
pub async fn enroll(opts: &EnrollOptions) -> Result<Credential, AgentError> {
    let key = KeyPair::generate().map_err(|e| AgentError::Enroll(e.to_string()))?;
    let csr = CertificateParams::default()
        .serialize_request(&key)
        .map_err(|e| AgentError::Enroll(e.to_string()))?;

    let server_ca = common::tls::certs_from_pem(&opts.server_ca_pem)?;
    let bind_addr = opts.bind_addr.unwrap_or(match opts.server_addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    });
    let mut endpoint = quinn::Endpoint::client(bind_addr).map_err(AgentError::Bind)?;
    // No client certificate: this connection may only enroll.
    endpoint.set_default_client_config(common::quic::client_config(&server_ca, None)?);
    let conn = endpoint
        .connect(opts.server_addr, &opts.server_name)?
        .await?;

    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::Enroll {
            token: opts.token.clone(),
            csr_der: csr.der().to_vec(),
        },
    )
    .await?;
    let reply = read_frame(&mut recv).await;
    // Read the server's close reason (if it rejected us) before closing ourselves.
    let rejection = match conn.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(close)) => {
            Some(String::from_utf8_lossy(&close.reason).into_owned())
        }
        _ => None,
    };
    conn.close(close_code::NORMAL.into(), b"enrolled");
    endpoint.wait_idle().await;

    match reply {
        Ok(Some(Message::Enrolled {
            agent_id,
            cert_pem,
            ca_pem,
            api_url,
        })) => {
            info!(%agent_id, "enrolled");
            Ok(Credential {
                agent_id,
                cert_pem,
                key_pem: key.serialize_pem(),
                ca_pem,
                server_addr: opts.server_addr,
                server_name: opts.server_name.clone(),
                api_url,
            })
        }
        Ok(Some(other)) => Err(AgentError::Unexpected(other)),
        Ok(None) => Err(AgentError::Enroll("server closed the stream".into())),
        // Rejections arrive as a connection close with a reason.
        Err(e) => Err(AgentError::Enroll(
            rejection.unwrap_or_else(|| e.to_string()),
        )),
    }
}

/// File in the state directory holding a pending enrollment.
pub const REQUEST_FILE: &str = "enroll-request.json";

/// A pending enrollment, written by `agent service install` and carried out
/// by the service itself on first start.
///
/// Why not enroll during install: the credential is protected with DPAPI
/// under the account that saves it. Install runs as the administrator who
/// typed the command, but the service runs as SYSTEM, which could not
/// decrypt an administrator's DPAPI blob. So the service enrolls, and the
/// credential is protected under SYSTEM. The token is single-use and this
/// file lives in the SYSTEM/Administrators-only state directory; it is
/// deleted once enrollment succeeds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub server_addr: SocketAddr,
    pub server_name: String,
    pub server_ca_pem: String,
    pub token: String,
}

impl std::fmt::Debug for EnrollRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrollRequest")
            .field("server_addr", &self.server_addr)
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl EnrollRequest {
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(REQUEST_FILE)
    }

    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        common::fs::write_file(&Self::path(state_dir), &json, true)
    }

    pub fn load(state_dir: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(Self::path(state_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn remove(state_dir: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(Self::path(state_dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn options(&self) -> EnrollOptions {
        EnrollOptions {
            server_addr: self.server_addr,
            server_name: self.server_name.clone(),
            server_ca_pem: self.server_ca_pem.clone(),
            token: self.token.clone(),
            bind_addr: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enroll_request_round_trips_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(EnrollRequest::load(dir.path()).unwrap(), None);
        let req = EnrollRequest {
            server_addr: "192.168.122.1:4433".parse().unwrap(),
            server_name: "localhost".into(),
            server_ca_pem: "-----BEGIN CERTIFICATE-----".into(),
            token: "secret-token".into(),
        };
        req.save(dir.path()).unwrap();
        assert_eq!(EnrollRequest::load(dir.path()).unwrap(), Some(req.clone()));
        assert!(!format!("{req:?}").contains("secret-token"));
        EnrollRequest::remove(dir.path()).unwrap();
        EnrollRequest::remove(dir.path()).unwrap(); // idempotent
        assert_eq!(EnrollRequest::load(dir.path()).unwrap(), None);
    }
}
