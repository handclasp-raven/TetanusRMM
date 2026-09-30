//! A direct QUIC connection between agent and viewer.
//!
//! Signaling happens over the sealed session (so the server relays it
//! without reading it):
//!
//! 1. The viewer gathers candidates for a fresh UDP socket and sends a
//!    `DirectOffer`: its candidates and the SHA-256 of a self-signed
//!    certificate made for this attempt.
//! 2. The agent gathers its own, **punches** towards each of the viewer's
//!    candidates ([`punch`]), starts a QUIC server on its socket
//!    ([`Listening`]) and sends a `DirectAnswer` with its candidates and
//!    certificate hash.
//! 3. The viewer connects to all of the agent's candidates at once
//!    ([`connect`]); the first to complete wins.
//!
//! Both ends pin the other's certificate hash, so the connection is
//! mutually authenticated by the session it was negotiated over; nobody
//! else can complete it. Nothing sent on it relies on that alone, though:
//! records and video on it are the same sealed ones the relay carries.
//!
//! **Punching.** A NAT only lets a packet in from an address that the host
//! behind it has sent to. The viewer's QUIC handshake opens the viewer's
//! NAT; the agent's punch opens the agent's. The punch is sent with a small
//! TTL ([`PUNCH_TTL`]) so it leaves the agent's NAT (creating the mapping)
//! but expires on the way: if it reached the viewer's NAT before the
//! viewer had sent anything, a NAT that tracks unsolicited packets (Linux
//! conntrack does) would reserve the port pair for it, then give the
//! viewer's handshake a different public port, and the punch would be
//! wasted. The agent punches before it answers, so the viewer's handshake
//! always arrives after the agent's NAT is open.
//!
//! This works through the ordinary NATs of homes and offices, which keep a
//! socket's public port the same whoever it sends to. A symmetric NAT
//! gives each destination its own port, so neither side can predict it and
//! the attempt fails; the session carries on over the relay.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use protocol::e2e::DirectHello;
use protocol::{read_frame, write_frame};
use ring::digest::{digest, SHA256};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tokio::task::JoinSet;
use tracing::debug;

use crate::gather::{Gathered, MAX_CANDIDATES};

/// ALPN of direct connections (never the server's).
pub const ALPN: &[u8] = b"rmm-direct/1";

/// TTL of punch packets: enough to leave the local network and its NAT,
/// not enough to reach the other peer's NAT (see the module docs).
pub const PUNCH_TTL: u32 = 2;

/// Punch packets per candidate (UDP may drop one).
const PUNCHES: usize = 2;

/// A direct path this quiet is dead: fall back to the relay. Short, since
/// video freezes until then.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(6);
pub const KEEP_ALIVE: Duration = Duration::from_secs(1);

/// Server name the viewer asks for; certificates are pinned, not named.
const SERVER_NAME: &str = "rmm-direct";

/// How a peer takes part in direct paths.
#[derive(Debug, Clone)]
pub struct DirectSettings {
    /// Try direct paths at all (the server can also turn them off).
    pub enabled: bool,
    /// Local address for the direct socket. `None`: the unspecified
    /// address of the server's family, with every interface a candidate.
    pub bind: Option<IpAddr>,
    /// STUN server to learn the public address from, instead of the one
    /// the server announces.
    pub stun: Option<SocketAddr>,
    /// Advertise exactly these candidates instead of gathering (e.g. a
    /// fixed port forward, or tests). The socket still binds `bind`.
    pub advertise: Option<Vec<SocketAddr>>,
    /// How long an attempt may take to connect.
    pub timeout: Duration,
    /// Viewer: stay on the relay this long before trying (the session
    /// starts on the relay either way).
    pub delay: Duration,
}

impl Default for DirectSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: None,
            stun: None,
            advertise: None,
            timeout: Duration::from_secs(10),
            delay: Duration::ZERO,
        }
    }
}

impl DirectSettings {
    /// Where to bind, for a server at `server`.
    pub fn bind_ip(&self, server: SocketAddr) -> IpAddr {
        self.bind.unwrap_or(match server {
            SocketAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        })
    }

    /// The STUN server: the configured one, else the server's responder.
    pub fn stun_server(&self, server: SocketAddr, stun_port: Option<u16>) -> Option<SocketAddr> {
        self.stun
            .or_else(|| stun_port.map(|port| SocketAddr::new(server.ip(), port)))
    }

    /// Gather candidates for a new attempt.
    pub async fn gather(
        &self,
        server: SocketAddr,
        stun_port: Option<u16>,
    ) -> std::io::Result<Gathered> {
        let mut gathered =
            crate::gather::gather(self.bind_ip(server), self.stun_server(server, stun_port))
                .await?;
        if let Some(advertise) = &self.advertise {
            gathered.candidates = advertise.clone();
        }
        Ok(gathered)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DirectError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("tls: {0}")]
    Tls(#[from] rustls::Error),
    #[error("no candidate answered within {0:?}")]
    TimedOut(Duration),
    #[error("no candidates to try")]
    NoCandidates,
    #[error("connection: {0}")]
    Connection(String),
}

/// A self-signed certificate for one attempt; its hash is sent over the
/// sealed session for the other end to pin.
pub struct Credentials {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub sha256: [u8; 32],
}

impl Credentials {
    pub fn generate() -> Self {
        let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()])
            .expect("self-signed certificate");
        let cert = certified.cert.der().clone();
        Self {
            sha256: fingerprint(&cert),
            key: PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
            cert,
        }
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

fn fingerprint(cert: &CertificateDer<'_>) -> [u8; 32] {
    digest(&SHA256, cert.as_ref())
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts exactly one certificate (by hash), as server or client.
#[derive(Debug)]
struct Pinned {
    sha256: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl Pinned {
    fn new(sha256: [u8; 32]) -> Arc<Self> {
        Arc::new(Self {
            sha256,
            algorithms: provider().signature_verification_algorithms,
        })
    }

    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if fingerprint(cert) == self.sha256 {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("fits in VarInt")))
        .keep_alive_interval(Some(KEEP_ALIVE));
    // As for the server connection (see `common::quic`): some Windows NIC
    // drivers drop segmented UDP batches.
    if cfg!(windows) {
        transport.enable_segmentation_offload(false);
    }
    Arc::new(transport)
}

fn server_config(own: &Credentials, peer: [u8; 32]) -> Result<quinn::ServerConfig, rustls::Error> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Pinned::new(peer))
        .with_single_cert(vec![own.cert.clone()], own.key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|e| rustls::Error::General(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(transport());
    Ok(config)
}

fn client_config(own: &Credentials, peer: [u8; 32]) -> Result<quinn::ClientConfig, rustls::Error> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Pinned::new(peer))
        .with_client_auth_cert(vec![own.cert.clone()], own.key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| rustls::Error::General(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(transport());
    Ok(config)
}

/// Open this socket's NAT towards `targets` (see the module docs).
pub fn punch(socket: &UdpSocket, targets: &[SocketAddr]) {
    let ttl = socket.ttl().unwrap_or(64);
    if let Err(e) = socket.set_ttl(PUNCH_TTL) {
        debug!("cannot lower TTL for punching: {e}");
    }
    let local_v4 = socket.local_addr().is_ok_and(|a| a.is_ipv4());
    // IP_TTL only applies to IPv4. IPv6 rarely has NAT, and a full hop
    // limit there only risks the remapping described above.
    let targets = targets
        .iter()
        .filter(|t| t.is_ipv4() == local_v4)
        .take(MAX_CANDIDATES);
    for target in targets {
        for _ in 0..PUNCHES {
            // Best effort: an unreachable candidate is expected.
            let _ = socket.send_to(b"rmm punch", target);
        }
    }
    let _ = socket.set_ttl(ttl);
}

/// An established direct path, with its control stream.
pub struct DirectLink {
    pub connection: quinn::Connection,
    /// Owns the socket; kept for the connection's lifetime.
    pub endpoint: quinn::Endpoint,
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}

impl DirectLink {
    /// The other peer's address on this path.
    pub fn remote(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    pub fn close(&self) {
        self.connection.close(0u32.into(), b"closed");
        self.endpoint.close(0u32.into(), b"closed");
    }
}

/// The agent's end of an attempt: punched, and listening.
pub struct Listening {
    endpoint: quinn::Endpoint,
    attempt: u32,
}

impl Listening {
    /// Punch towards the viewer's `candidates`, then listen on the
    /// gathered socket for the viewer presenting `viewer_sha256`.
    pub fn start(
        socket: UdpSocket,
        own: &Credentials,
        viewer_sha256: [u8; 32],
        candidates: &[SocketAddr],
        attempt: u32,
    ) -> Result<Self, DirectError> {
        punch(&socket, candidates);
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config(own, viewer_sha256)?),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok(Self { endpoint, attempt })
    }

    /// Wait up to `timeout` for the viewer. Several of its connection
    /// attempts may get through (one per candidate pair); the one it opens
    /// the control stream on is the one it chose.
    pub async fn accept(self, timeout: Duration) -> Result<DirectLink, DirectError> {
        let endpoint = self.endpoint.clone();
        let attempt = self.attempt;
        let result = tokio::time::timeout(timeout, async move {
            let mut pending = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = endpoint.accept() => {
                        let Some(incoming) = incoming else {
                            return Err(DirectError::Connection("endpoint closed".into()));
                        };
                        pending.spawn(async move {
                            let connection = incoming.await.map_err(|e| DirectError::Connection(e.to_string()))?;
                            let (send, mut recv) = connection.accept_bi().await.map_err(|e| DirectError::Connection(e.to_string()))?;
                            match read_frame::<_, DirectHello>(&mut recv).await {
                                Ok(Some(hello)) if hello.attempt == attempt => Ok((connection, send, recv)),
                                _ => Err(DirectError::Connection("bad hello".into())),
                            }
                        });
                    }
                    Some(done) = pending.join_next(), if !pending.is_empty() => {
                        match done {
                            Ok(Ok((connection, send, recv))) => return Ok((connection, send, recv)),
                            Ok(Err(e)) => debug!("direct connection attempt failed: {e}"),
                            Err(_) => {}
                        }
                    }
                }
            }
        })
        .await;
        match result {
            Ok(Ok((connection, send, recv))) => Ok(DirectLink {
                connection,
                endpoint: self.endpoint,
                send,
                recv,
            }),
            Ok(Err(e)) => {
                self.endpoint.close(0u32.into(), b"failed");
                Err(e)
            }
            Err(_) => {
                self.endpoint.close(0u32.into(), b"timed out");
                Err(DirectError::TimedOut(timeout))
            }
        }
    }
}

/// The viewer's end: connect from the gathered socket to every agent
/// candidate at once, presenting `own` and pinning `agent_sha256`; the
/// first connection up wins and gets the control stream.
pub async fn connect(
    socket: UdpSocket,
    own: &Credentials,
    agent_sha256: [u8; 32],
    candidates: &[SocketAddr],
    attempt: u32,
    timeout: Duration,
) -> Result<DirectLink, DirectError> {
    let local_v4 = socket.local_addr()?.is_ipv4();
    let candidates: Vec<SocketAddr> = candidates
        .iter()
        .copied()
        .filter(|c| c.is_ipv4() == local_v4)
        .take(MAX_CANDIDATES)
        .collect();
    if candidates.is_empty() {
        return Err(DirectError::NoCandidates);
    }
    let mut endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config(own, agent_sha256)?);

    let mut racing = JoinSet::new();
    for candidate in candidates {
        let connecting = match endpoint.connect(candidate, SERVER_NAME) {
            Ok(connecting) => connecting,
            Err(e) => {
                debug!(%candidate, "cannot connect: {e}");
                continue;
            }
        };
        racing.spawn(async move { (candidate, connecting.await) });
    }
    let first = tokio::time::timeout(timeout, async {
        while let Some(done) = racing.join_next().await {
            match done {
                Ok((candidate, Ok(connection))) => {
                    debug!(%candidate, "direct connection up");
                    return Some(connection);
                }
                Ok((candidate, Err(e))) => debug!(%candidate, "direct connection failed: {e}"),
                Err(_) => {}
            }
        }
        None
    })
    .await;
    // Losers are abandoned (the agent drops what it accepted of them).
    racing.abort_all();
    let connection = match first {
        Ok(Some(connection)) => connection,
        Ok(None) => {
            endpoint.close(0u32.into(), b"failed");
            return Err(DirectError::Connection("every candidate failed".into()));
        }
        Err(_) => {
            endpoint.close(0u32.into(), b"timed out");
            return Err(DirectError::TimedOut(timeout));
        }
    };
    let opened = async {
        let (mut send, recv) = connection.open_bi().await.map_err(|e| e.to_string())?;
        write_frame(&mut send, &DirectHello { attempt })
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((send, recv))
    };
    match opened.await {
        Ok((send, recv)) => Ok(DirectLink {
            connection,
            endpoint,
            send,
            recv,
        }),
        Err(e) => {
            endpoint.close(0u32.into(), b"failed");
            Err(DirectError::Connection(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gather::gather;

    async fn loopback() -> Gathered {
        gather("127.0.0.1".parse().unwrap(), None).await.unwrap()
    }

    #[tokio::test]
    async fn peers_connect_with_pinned_certificates() {
        let (agent_creds, viewer_creds) = (Credentials::generate(), Credentials::generate());
        let (agent, viewer) = (loopback().await, loopback().await);
        let agent_candidates = agent.candidates.clone();
        let listening = Listening::start(
            agent.socket,
            &agent_creds,
            viewer_creds.sha256,
            &viewer.candidates,
            7,
        )
        .unwrap();
        let accept = tokio::spawn(listening.accept(Duration::from_secs(5)));
        let mut v = connect(
            viewer.socket,
            &viewer_creds,
            agent_creds.sha256,
            &agent_candidates,
            7,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let mut a = accept.await.unwrap().unwrap();
        write_frame(&mut a.send, &42u32).await.unwrap();
        assert_eq!(read_frame::<_, u32>(&mut v.recv).await.unwrap(), Some(42));
        assert_eq!(v.remote(), agent_candidates[0]);
    }

    #[tokio::test]
    async fn a_peer_with_another_certificate_is_refused() {
        let (agent_creds, viewer_creds, impostor) = (
            Credentials::generate(),
            Credentials::generate(),
            Credentials::generate(),
        );
        let (agent, viewer) = (loopback().await, loopback().await);
        let agent_candidates = agent.candidates.clone();
        let listening = Listening::start(
            agent.socket,
            &agent_creds,
            viewer_creds.sha256,
            &viewer.candidates,
            1,
        )
        .unwrap();
        let accept = tokio::spawn(listening.accept(Duration::from_secs(2)));
        let result = connect(
            viewer.socket,
            &impostor,
            agent_creds.sha256,
            &agent_candidates,
            1,
            Duration::from_secs(2),
        )
        .await;
        // The handshake may complete on the client side before the server
        // rejects the certificate; either way the agent never accepts it.
        drop(result);
        assert!(matches!(
            accept.await.unwrap(),
            Err(DirectError::TimedOut(_))
        ));

        // And the viewer refuses an agent it did not pin.
        let (agent, viewer) = (loopback().await, loopback().await);
        let agent_candidates = agent.candidates.clone();
        let listening = Listening::start(
            agent.socket,
            &impostor,
            viewer_creds.sha256,
            &viewer.candidates,
            1,
        )
        .unwrap();
        let _accept = tokio::spawn(listening.accept(Duration::from_secs(2)));
        assert!(connect(
            viewer.socket,
            &viewer_creds,
            agent_creds.sha256,
            &agent_candidates,
            1,
            Duration::from_secs(2),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn an_unreachable_candidate_times_out() {
        let creds = Credentials::generate();
        let viewer = loopback().await;
        // Bound, never answers.
        let black_hole = UdpSocket::bind("127.0.0.1:0").unwrap();
        let result = connect(
            viewer.socket,
            &creds,
            creds.sha256,
            &[black_hole.local_addr().unwrap()],
            1,
            Duration::from_millis(500),
        )
        .await;
        assert!(
            matches!(result, Err(DirectError::TimedOut(_))),
            "{:?}",
            result.err()
        );
    }

    #[test]
    fn punching_restores_the_ttl() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = socket.ttl().unwrap();
        let target = UdpSocket::bind("127.0.0.1:0").unwrap();
        punch(&socket, &[target.local_addr().unwrap()]);
        assert_eq!(socket.ttl().unwrap(), before);
        let mut buf = [0u8; 32];
        let (n, _) = target.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"rmm punch");
    }
}
