//! QUIC listener that accepts mutually-authenticated agent connections.
//!
//! Each connection gets its own task. The agent opens one bidirectional
//! control stream, sends [`Message::Hello`], then [`Message::Heartbeat`]s,
//! each answered with a [`Message::HeartbeatAck`].

use std::net::SocketAddr;

use common::Identity;
use protocol::{close_code, read_frame, write_frame, FrameError, Message};
use quinn::rustls::pki_types::CertificateDer;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, info_span, warn, Instrument};

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
    #[error("connection has no verified client certificate")]
    NoClientCert,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}

pub struct Server {
    endpoint: quinn::Endpoint,
    events: Option<UnboundedSender<ServerEvent>>,
}

impl Server {
    pub fn bind(config: ServerConfig) -> Result<Self, ServerError> {
        let quic = common::quic::server_config(&config.identity, &config.client_ca)?;
        let endpoint = quinn::Endpoint::server(quic, config.listen)?;
        Ok(Self {
            endpoint,
            events: None,
        })
    }

    /// Report connection activity on `events` as well as logging it.
    pub fn with_events(mut self, events: UnboundedSender<ServerEvent>) -> Self {
        self.events = Some(events);
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
            let events = self.events.clone();
            tokio::spawn(
                async move {
                    match handle_connection(incoming, events).await {
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

async fn handle_connection(
    incoming: quinn::Incoming,
    events: Option<UnboundedSender<ServerEvent>>,
) -> Result<(), ConnError> {
    // The handshake fails here if the agent did not present a certificate that
    // chains to the client CA (see common::quic::server_config).
    let conn = incoming.await?;
    let has_client_cert = conn
        .peer_identity()
        .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
        .is_some_and(|chain| !chain.is_empty());
    if !has_client_cert {
        // Unreachable with the verifier in place; kept as a second line of defence.
        conn.close(
            close_code::PROTOCOL_ERROR.into(),
            b"client certificate required",
        );
        return Err(ConnError::NoClientCert);
    }
    let conn_id = conn.stable_id();
    info!(conn_id, "client certificate verified");

    let result = serve_control_stream(&conn, conn_id, &events).await;
    if let Err(ConnError::Protocol(reason)) = &result {
        conn.close(close_code::PROTOCOL_ERROR.into(), reason.as_bytes());
    }
    result
}

async fn serve_control_stream(
    conn: &quinn::Connection,
    conn_id: usize,
    events: &Option<UnboundedSender<ServerEvent>>,
) -> Result<(), ConnError> {
    let emit = |event| {
        if let Some(tx) = events {
            let _ = tx.send(event);
        }
    };

    let (mut send, mut recv) = conn.accept_bi().await?;

    let agent_id = match read_frame(&mut recv).await? {
        Some(Message::Hello { agent_id, version }) => {
            info!(conn_id, %agent_id, version, "hello");
            emit(ServerEvent::Hello {
                conn_id,
                agent_id: agent_id.clone(),
                version,
            });
            agent_id
        }
        Some(_) => return Err(ConnError::Protocol("expected Hello")),
        None => return Ok(()),
    };

    while let Some(msg) = read_frame(&mut recv).await? {
        match msg {
            Message::Heartbeat { ts, seq } => {
                let remote = conn.remote_address();
                debug!(%agent_id, seq, ts, %remote, "heartbeat");
                emit(ServerEvent::Heartbeat {
                    conn_id,
                    agent_id: agent_id.clone(),
                    seq,
                    remote,
                });
                write_frame(&mut send, &Message::HeartbeatAck { seq }).await?;
            }
            other => {
                warn!(%agent_id, ?other, "unexpected message on control stream");
                return Err(ConnError::Protocol("unexpected message"));
            }
        }
    }
    Ok(())
}
