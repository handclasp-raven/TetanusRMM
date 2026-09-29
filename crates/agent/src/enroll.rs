//! First-run enrollment: trade a one-time token for a client certificate.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use protocol::{close_code, read_frame, write_frame, Message};
use rcgen::{CertificateParams, KeyPair};
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
