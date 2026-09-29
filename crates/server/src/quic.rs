//! QUIC listener for agents.
//!
//! Each connection gets its own task and opens one bidirectional control
//! stream. What is allowed on it depends on whether the agent presented a
//! client certificate during the handshake:
//!
//! | Client cert | First message | Outcome |
//! |---|---|---|
//! | yes | `Hello` | Accepted if the cert is pinned to that agent id in the registry, then `Heartbeat`/`HeartbeatAck` |
//! | no  | `Enroll` | Token consumed, certificate issued, `Enrolled` sent, connection ends |
//! | anything else | | Closed with `PROTOCOL_ERROR` or `UNAUTHORIZED` |
//!
//! Without a registry (tests only), any certificate from the CA may say
//! `Hello` and enrollment is unavailable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::Identity;
use protocol::{close_code, read_frame, write_frame, FrameError, Message};
use quinn::rustls::pki_types::CertificateDer;
use ring::digest::{digest, SHA256};
use sqlx::PgPool;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::enroll::{self, AgentCa, EnrollError};
use crate::registry;

/// How long to wait for an enrolling agent to read its certificate and hang up.
const ENROLL_LINGER: Duration = Duration::from_secs(10);

pub struct ServerConfig {
    pub listen: SocketAddr,
    /// Certificate the server presents to agents.
    pub identity: Identity,
    /// CA(s) that agent client certificates must chain to.
    pub client_ca: Vec<CertificateDer<'static>>,
}

/// What the server observed, for tests and (later) the agent registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    Hello {
        conn_id: usize,
        agent_id: String,
        version: u32,
    },
    Heartbeat {
        conn_id: usize,
        agent_id: String,
        seq: u64,
        /// Agent address this heartbeat arrived from. Changes on migration.
        remote: SocketAddr,
    },
    Enrolled {
        conn_id: usize,
        agent_id: String,
    },
}

/// Database-backed agent registry plus what enrollment needs.
pub struct Registry {
    pub pool: PgPool,
    /// Internal CA that signs agent certificates.
    pub ca: AgentCa,
    /// Public base URL of the HTTPS API, handed to agents at enrollment.
    pub api_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Tls(#[from] common::TlsError),
    #[error("binding QUIC endpoint: {0}")]
    Bind(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
enum ConnError {
    #[error(transparent)]
    Connection(#[from] quinn::ConnectionError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

pub struct Server {
    endpoint: quinn::Endpoint,
    events: Option<UnboundedSender<ServerEvent>>,
    registry: Option<Arc<Registry>>,
}

/// Per-connection context shared with the connection task.
#[derive(Clone)]
struct Hooks {
    events: Option<UnboundedSender<ServerEvent>>,
    registry: Option<Arc<Registry>>,
}

impl Hooks {
    fn emit(&self, event: ServerEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }
}

impl Server {
    pub fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        let quic = common::quic::server_config(&config.identity, &config.client_ca)?;
        let endpoint = quinn::Endpoint::server(quic, config.listen)?;
        Ok(Self {
            endpoint,
            events: None,
            registry: None,
        })
    }

    /// Report connection activity on `events` as well as logging it.
    pub fn with_events(mut self, events: UnboundedSender<ServerEvent>) -> Self {
        self.events = Some(events);
        self
    }

    /// Authenticate agents against the database registry and enable enrollment.
    pub fn with_registry(mut self, registry: Registry) -> Self {
        self.registry = Some(Arc::new(registry));
        self
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Accept connections until the endpoint is closed.
    pub async fn run(&self) {
        info!(addr = ?self.endpoint.local_addr().ok(), "listening for agents");
        while let Some(incoming) = self.endpoint.accept().await {
            let span = info_span!("agent_conn", remote = %incoming.remote_address());
            let hooks = Hooks {
                events: self.events.clone(),
                registry: self.registry.clone(),
            };
            tokio::spawn(
                async move {
                    match handle_connection(incoming, hooks).await {
                        Ok(()) => info!("agent disconnected"),
                        Err(e) => info!("agent connection ended: {e}"),
                    }
                }
                .instrument(span),
            );
        }
    }

    /// Stop accepting and close all connections.
    pub fn close(&self) {
        self.endpoint
            .close(close_code::NORMAL.into(), b"server shutting down");
    }
}

async fn handle_connection(incoming: quinn::Incoming, hooks: Hooks) -> Result<(), ConnError> {
    // The handshake fails here if the agent presented a certificate that does
    // not chain to the client CA (see common::quic::server_config).
    let conn = incoming.await?;
    let fingerprint = conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|chain| chain.first().map(|leaf| cert_fingerprint(leaf)));
    let conn_id = conn.stable_id();
    match &fingerprint {
        Some(fp) => info!(conn_id, fingerprint = %fp, "client certificate verified"),
        None => info!(conn_id, "connection without client certificate"),
    }

    let result = dispatch(&conn, conn_id, fingerprint.as_deref(), &hooks).await;
    match &result {
        Err(ConnError::Protocol(reason)) => {
            conn.close(close_code::PROTOCOL_ERROR.into(), reason.as_bytes());
        }
        Err(ConnError::Unauthorized(reason)) => {
            warn!(conn_id, "rejected: {reason}");
            conn.close(close_code::UNAUTHORIZED.into(), reason.as_bytes());
        }
        Err(ConnError::Db(e)) => {
            error!(conn_id, "database error: {e}");
            conn.close(close_code::PROTOCOL_ERROR.into(), b"server error");
        }
        _ => {}
    }
    result
}

async fn dispatch(
    conn: &quinn::Connection,
    conn_id: usize,
    fingerprint: Option<&str>,
    hooks: &Hooks,
) -> Result<(), ConnError> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    match (read_frame(&mut recv).await?, fingerprint) {
        (None, _) => Ok(()),
        (Some(Message::Hello { agent_id, version }), Some(fp)) => {
            if let Some(registry) = &hooks.registry {
                if !registry::authenticate_hello(&registry.pool, &agent_id, fp).await? {
                    return Err(ConnError::Unauthorized(
                        "certificate is not registered to this agent",
                    ));
                }
            }
            info!(conn_id, %agent_id, version, "hello");
            hooks.emit(ServerEvent::Hello {
                conn_id,
                agent_id: agent_id.clone(),
                version,
            });
            heartbeat_loop(conn, conn_id, &agent_id, &mut send, &mut recv, hooks).await
        }
        (Some(Message::Hello { .. }), None) => {
            Err(ConnError::Unauthorized("client certificate required"))
        }
        (Some(Message::Enroll { token, csr_der }), None) => {
            let registry = hooks
                .registry
                .as_ref()
                .ok_or(ConnError::Unauthorized("enrollment is not available"))?;
            let enrolled =
                match enroll::enroll(&registry.pool, &registry.ca, &token, &csr_der).await {
                    Ok(enrolled) => enrolled,
                    Err(EnrollError::InvalidToken) => {
                        return Err(ConnError::Unauthorized("invalid enrollment token"))
                    }
                    Err(EnrollError::Ca(e)) => {
                        warn!(conn_id, "enrollment CSR rejected: {e}");
                        return Err(ConnError::Protocol("invalid certificate signing request"));
                    }
                    Err(EnrollError::Db(e)) => return Err(e.into()),
                };
            info!(conn_id, agent_id = %enrolled.agent_id, fingerprint = %enrolled.fingerprint, "agent enrolled");
            write_frame(
                &mut send,
                &Message::Enrolled {
                    agent_id: enrolled.agent_id.clone(),
                    cert_pem: enrolled.cert_pem,
                    ca_pem: registry.ca.ca_pem().to_owned(),
                    api_url: registry.api_url.clone(),
                },
            )
            .await?;
            let _ = send.finish();
            hooks.emit(ServerEvent::Enrolled {
                conn_id,
                agent_id: enrolled.agent_id,
            });
            // Let the agent read the reply and close; closing first could
            // discard the unacknowledged certificate.
            let _ = tokio::time::timeout(ENROLL_LINGER, conn.closed()).await;
            Ok(())
        }
        (Some(Message::Enroll { .. }), Some(_)) => {
            Err(ConnError::Protocol("already enrolled; send Hello"))
        }
        (Some(_), _) => Err(ConnError::Protocol("expected Hello or Enroll")),
    }
}

async fn heartbeat_loop(
    conn: &quinn::Connection,
    conn_id: usize,
    agent_id: &str,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    hooks: &Hooks,
) -> Result<(), ConnError> {
    while let Some(msg) = read_frame(recv).await? {
        match msg {
            Message::Heartbeat { ts, seq } => {
                let remote = conn.remote_address();
                debug!(%agent_id, seq, ts, %remote, "heartbeat");
                hooks.emit(ServerEvent::Heartbeat {
                    conn_id,
                    agent_id: agent_id.to_owned(),
                    seq,
                    remote,
                });
                if let Some(registry) = &hooks.registry {
                    // A registry hiccup should not disconnect an authenticated agent.
                    if let Err(e) = registry::touch(&registry.pool, agent_id).await {
                        warn!(%agent_id, "registry update failed: {e}");
                    }
                }
                write_frame(send, &Message::HeartbeatAck { seq }).await?;
            }
            other => {
                warn!(%agent_id, ?other, "unexpected message on control stream");
                return Err(ConnError::Protocol("unexpected message"));
            }
        }
    }
    Ok(())
}

/// Hex SHA-256 of a DER certificate.
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(digest(&SHA256, cert.as_ref()))
}
