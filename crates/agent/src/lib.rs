//! Agent side of the control connection: connect with a client certificate,
//! send [`Message::Hello`], then heartbeat until the connection drops.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::Identity;
use protocol::{close_code, read_frame, write_frame, FrameError, Message, PROTOCOL_VERSION};
use quinn::rustls::pki_types::CertificateDer;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub struct AgentConfig {
    pub server_addr: SocketAddr,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    pub agent_id: String,
    /// CA(s) the server certificate must chain to.
    pub server_ca: Vec<CertificateDer<'static>>,
    /// Client certificate presented to the server.
    pub identity: Identity,
    pub heartbeat_interval: Duration,
    /// Local UDP address to bind. `None` binds the unspecified address of the
    /// server's address family, which is what production agents should use
    /// (see [`AgentSession::rebind`] for why).
    pub bind_addr: Option<SocketAddr>,
}

/// What the agent observed, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Ack { seq: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Tls(#[from] common::TlsError),
    #[error("binding local UDP socket: {0}")]
    Bind(#[source] std::io::Error),
    #[error(transparent)]
    Connect(#[from] quinn::ConnectError),
    #[error(transparent)]
    Connection(#[from] quinn::ConnectionError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("server closed the control stream")]
    StreamClosed,
    #[error("unexpected message from server: {0:?}")]
    Unexpected(Message),
}

/// An established, authenticated connection to the server.
pub struct AgentSession {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    agent_id: String,
    heartbeat_interval: Duration,
}

/// Connect to the server and complete the mutual-TLS handshake.
pub async fn connect(config: &AgentConfig) -> Result<AgentSession, AgentError> {
    let bind_addr = config.bind_addr.unwrap_or(match config.server_addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    });
    let mut endpoint = quinn::Endpoint::client(bind_addr).map_err(AgentError::Bind)?;
    endpoint.set_default_client_config(common::quic::client_config(
        &config.server_ca,
        Some(&config.identity),
    )?);

    let connection = endpoint
        .connect(config.server_addr, &config.server_name)?
        .await?;
    info!(server = %config.server_addr, "connected");
    Ok(AgentSession {
        endpoint,
        connection,
        agent_id: config.agent_id.clone(),
        heartbeat_interval: config.heartbeat_interval,
    })
}

impl AgentSession {
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Move the connection onto a new local UDP socket without reconnecting.
    ///
    /// CONNECTION MIGRATION (agent half).
    ///
    /// Two cases keep an agent connected across a local address change:
    ///
    /// 1. Passive (the normal case, no code needed). The agent binds the
    ///    unspecified address (`0.0.0.0:0` / `[::]:0`, see `connect`), so the
    ///    OS picks the source address per packet from the current routing
    ///    table. When Wi-Fi drops and Ethernet comes up, the next packet (a
    ///    heartbeat, or a keep-alive from `common::quic::KEEP_ALIVE_INTERVAL`)
    ///    simply leaves from the new address. Same for NAT rebinding. The
    ///    server, which has migration enabled in `common::quic::server_config`,
    ///    validates the new path and keeps the connection.
    ///
    /// 2. Active (this method). If the socket itself becomes unusable, e.g. it
    ///    was bound to a specific interface address that went away, the caller
    ///    binds a fresh socket and hands it here. quinn switches the endpoint to
    ///    it and the server migrates the connection the same way.
    ///
    /// Either way the control stream, heartbeat sequence and TLS session carry
    /// on; there is no new handshake and no new Hello. Detecting interface
    /// changes to trigger case 2 automatically is not done yet.
    pub fn rebind(&self, socket: std::net::UdpSocket) -> std::io::Result<()> {
        self.endpoint.rebind(socket)
    }

    /// Send Hello, then heartbeat until the connection or stream ends.
    ///
    /// Only returns on failure; a healthy session runs forever.
    pub async fn run(&self, events: Option<UnboundedSender<AgentEvent>>) -> Result<(), AgentError> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        write_frame(
            &mut send,
            &Message::Hello {
                agent_id: self.agent_id.clone(),
                version: PROTOCOL_VERSION,
            },
        )
        .await?;

        let heartbeats = async {
            let mut ticker = tokio::time::interval(self.heartbeat_interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            for seq in 0u64.. {
                ticker.tick().await;
                write_frame(
                    &mut send,
                    &Message::Heartbeat {
                        ts: unix_millis(),
                        seq,
                    },
                )
                .await?;
            }
            unreachable!("heartbeat sequence exhausted")
        };

        let acks = async {
            loop {
                match read_frame(&mut recv).await? {
                    Some(Message::HeartbeatAck { seq }) => {
                        info!(seq, "heartbeat acked");
                        if let Some(tx) = &events {
                            let _ = tx.send(AgentEvent::Ack { seq });
                        }
                    }
                    Some(other) => return Err(AgentError::Unexpected(other)),
                    None => return Err(AgentError::StreamClosed),
                }
            }
        };

        // Each branch only finishes on error. read_frame is not cancel-safe,
        // which is fine: the loser is dropped along with the session.
        let result: Result<(), AgentError> = tokio::select! {
            res = heartbeats => res,
            res = acks => res,
        };
        if let Err(AgentError::Unexpected(msg)) = &result {
            warn!(?msg, "closing connection after protocol violation");
            self.connection
                .close(close_code::PROTOCOL_ERROR.into(), b"unexpected message");
        }
        result
    }

    /// Close the connection cleanly.
    pub fn close(&self) {
        self.connection
            .close(close_code::NORMAL.into(), b"agent shutting down");
    }
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
