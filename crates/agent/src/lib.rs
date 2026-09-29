//! RMM agent.
//!
//! - Control connection (this module): connect with the enrolled client
//!   certificate, send [`Message::Hello`], then heartbeat until it drops.
//! - [`enroll`]: first-run exchange of a one-time token for a certificate.
//! - [`credstore`]: the certificate and key, protected at rest.
//! - [`update`] and [`updater`]: signed self-update.
//! - [`telemetry`]: health samples sent on each heartbeat.
//! - [`core`]: the connect/heartbeat/update loop shared by console and service mode.
//! - [`session`]: when to (re)start the helper in the user's session.
//! - [`paths`]: where the agent keeps its files.
//! - `win` (Windows only): the service, the session helper and its tray icon.

pub mod core;
pub mod credstore;
pub mod enroll;
pub mod media;
pub mod paths;
pub mod session;
pub mod telemetry;
pub mod update;
pub mod updater;
#[cfg(windows)]
pub mod win;

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::Identity;
use protocol::media::VideoPayload;
use protocol::{close_code, read_frame, write_frame, FrameError, Message, PROTOCOL_VERSION};
use quinn::rustls::pki_types::CertificateDer;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use crate::media::source::{MediaCommand, MediaEvent, MediaLink};
use crate::telemetry::TelemetrySource;

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
    /// Telemetry to attach to heartbeats, if any.
    pub telemetry: Option<Arc<dyn TelemetrySource>>,
    /// Screen source for streaming, if any (Windows: the session helper).
    pub media: Option<Arc<MediaLink>>,
}

/// What the agent observed, for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Ack {
        seq: u64,
    },
    /// A streaming command arrived from the server.
    Media(MediaCommand),
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
    #[error("enrollment failed: {0}")]
    Enroll(String),
}

/// An established, authenticated connection to the server.
pub struct AgentSession {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    agent_id: String,
    heartbeat_interval: Duration,
    telemetry: Option<Arc<dyn TelemetrySource>>,
    media: Option<Arc<MediaLink>>,
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
        telemetry: config.telemetry.clone(),
        media: config.media.clone(),
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

    /// Send Hello, then heartbeat until the connection or stream ends,
    /// serving the server's streaming commands if a media source is attached.
    ///
    /// Only returns on failure; a healthy session runs forever.
    pub async fn run(&self, events: Option<UnboundedSender<AgentEvent>>) -> Result<(), AgentError> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        // Everything written on the control stream goes through one channel,
        // so heartbeats, monitor lists and replies never interleave.
        let (outbox_tx, mut outbox) = mpsc::unbounded_channel::<Message>();
        let _ = outbox_tx.send(Message::Hello {
            agent_id: self.agent_id.clone(),
            version: PROTOCOL_VERSION,
        });
        if let Some(media) = &self.media {
            // Fresh connection, fresh start: any stream from a previous
            // connection has no viewers any more.
            let _ = media.commands.send(MediaCommand::Stop);
        }

        let writer = async {
            while let Some(msg) = outbox.recv().await {
                write_frame(&mut send, &msg).await?;
            }
            Ok::<(), AgentError>(())
        };

        let heartbeats = async {
            let mut ticker = tokio::time::interval(self.heartbeat_interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            let mut seq = 0u64;
            loop {
                ticker.tick().await;
                let telemetry = match &self.telemetry {
                    Some(source) => {
                        let source = source.clone();
                        tokio::task::spawn_blocking(move || source.sample())
                            .await
                            .ok()
                    }
                    None => None,
                };
                let heartbeat = Message::Heartbeat {
                    ts: unix_millis(),
                    seq,
                    telemetry,
                };
                if outbox_tx.send(heartbeat).is_err() {
                    // The writer has stopped; it reports why.
                    return Ok::<(), AgentError>(());
                }
                seq += 1;
            }
        };

        let reader = async {
            loop {
                let msg = match read_frame(&mut recv).await? {
                    Some(msg) => msg,
                    None => return Err(AgentError::StreamClosed),
                };
                let command = match msg {
                    Message::HeartbeatAck { seq } => {
                        info!(seq, "heartbeat acked");
                        if let Some(tx) = &events {
                            let _ = tx.send(AgentEvent::Ack { seq });
                        }
                        continue;
                    }
                    Message::ListMonitors => MediaCommand::ListMonitors,
                    Message::StartStream { monitor } => MediaCommand::Start { monitor },
                    Message::StopStream => MediaCommand::Stop,
                    Message::RequestKeyframe => MediaCommand::ForceKeyframe,
                    other => return Err(AgentError::Unexpected(other)),
                };
                info!(?command, "stream command from server");
                if let Some(tx) = &events {
                    let _ = tx.send(AgentEvent::Media(command.clone()));
                }
                match &self.media {
                    Some(media) => {
                        let _ = media.commands.send(command);
                    }
                    None => warn!(
                        "server asked for screen streaming, but this agent has no capture source"
                    ),
                }
            }
        };

        // Monitor lists go on the control stream; frames on a video stream,
        // opened on the first frame and reused for the connection.
        let pump = async {
            let Some(media) = &self.media else {
                return std::future::pending::<Result<(), AgentError>>().await;
            };
            let mut source = media.events.lock().await;
            let mut video: Option<quinn::SendStream> = None;
            let mut seq = 0u64;
            while let Some(event) = source.recv().await {
                match event {
                    MediaEvent::Monitors(monitors) => {
                        let _ = outbox_tx.send(Message::MonitorList { monitors });
                    }
                    MediaEvent::Frame(frame) => {
                        let stream = match &mut video {
                            Some(stream) => stream,
                            None => {
                                info!(monitor = frame.monitor, "opening video stream to server");
                                video.insert(self.connection.open_uni().await?)
                            }
                        };
                        let payload = VideoPayload {
                            monitor: frame.monitor,
                            pts_us: frame.pts_us,
                            width: frame.width,
                            height: frame.height,
                            h264: frame.h264,
                        };
                        write_frame(stream, &payload.to_frame(seq, frame.keyframe)).await?;
                        seq += 1;
                    }
                }
            }
            // The source went away; keep the control connection up anyway.
            std::future::pending().await
        };

        // Each branch only finishes on error. read_frame is not cancel-safe,
        // which is fine: the losers are dropped along with the session.
        let result: Result<(), AgentError> = tokio::select! {
            res = writer => res,
            res = heartbeats => res,
            res = reader => res,
            res = pump => res,
        };
        if let Err(AgentError::Unexpected(msg)) = &result {
            warn!(?msg, "closing connection after protocol violation");
            self.connection
                .close(close_code::PROTOCOL_ERROR.into(), b"unexpected message");
        }
        result
    }

    /// Why the connection closed, if it has.
    pub fn close_reason(&self) -> Option<quinn::ConnectionError> {
        self.connection.close_reason()
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
