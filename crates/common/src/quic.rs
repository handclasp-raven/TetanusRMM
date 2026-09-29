//! QUIC endpoint configuration for mutual TLS between agent and server.
//!
//! Both sides are TLS 1.3 only (QUIC requires it) and negotiate
//! [`protocol::ALPN`]. The server requires a client certificate that chains to
//! the configured CA; the agent verifies the server certificate against the
//! same CA rather than the system trust store.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::server::WebPkiClientVerifier;
use rustls::RootCertStore;
use rustls_pki_types::CertificateDer;

use crate::tls::{Identity, TlsError};

/// A connection with no traffic for this long is considered dead.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the agent sends QUIC PING frames when otherwise idle.
///
/// Heartbeats normally keep the connection busy, but keep-alives also keep NAT
/// bindings warm and make a path change show up quickly.
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn root_store(ca: &[CertificateDer<'static>]) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    for cert in ca {
        roots.add(cert.clone())?;
    }
    Ok(roots)
}

fn transport(keep_alive: Option<Duration>) -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(
            IDLE_TIMEOUT
                .try_into()
                .expect("idle timeout fits in VarInt"),
        ))
        .keep_alive_interval(keep_alive);
    transport
}

/// Server-side QUIC config: presents `identity`, and requires every client to
/// present a certificate chaining to one of `client_ca`.
pub fn server_config(
    identity: &Identity,
    client_ca: &[CertificateDer<'static>],
) -> Result<quinn::ServerConfig, TlsError> {
    let provider = provider();
    // WebPkiClientVerifier rejects the handshake if the client sends no
    // certificate or one that does not chain to `client_ca`.
    let verifier = WebPkiClientVerifier::builder_with_provider(
        Arc::new(root_store(client_ca)?),
        provider.clone(),
    )
    .build()?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.cert_chain.clone(), identity.key.clone_key())?;
    tls.alpn_protocols = vec![protocol::ALPN.to_vec()];

    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport(None)));

    // CONNECTION MIGRATION (server half).
    //
    // QUIC identifies a connection by connection ID, not by the UDP 4-tuple.
    // With migration enabled, when packets for an existing connection arrive
    // from a new client address (the agent moved from Wi-Fi to Ethernet, got a
    // new DHCP lease, was NAT-rebound, or rebound its socket), quinn validates
    // the new path with PATH_CHALLENGE/PATH_RESPONSE and then carries on on the
    // same connection: same streams, same TLS session, no new Hello. quinn
    // enables this by default; it is set explicitly so nobody turns it off
    // without realising agents depend on it.
    //
    // The agent half is in `agent::AgentSession::rebind`.
    config.migration(true);
    Ok(config)
}

/// Client-side QUIC config: trusts only `server_ca`, and presents `identity`
/// as the client certificate when one is given.
pub fn client_config(
    server_ca: &[CertificateDer<'static>],
    identity: Option<&Identity>,
) -> Result<quinn::ClientConfig, TlsError> {
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(root_store(server_ca)?);
    let mut tls = match identity {
        Some(id) => builder.with_client_auth_cert(id.cert_chain.clone(), id.key.clone_key())?,
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![protocol::ALPN.to_vec()];

    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport(Some(KEEP_ALIVE_INTERVAL))));
    Ok(config)
}
