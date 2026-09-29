//! RMM agent.
//!
//! - Control connection (this module): connect with the enrolled client
//!   certificate, send [`Message::Hello`], then heartbeat until it drops.
//! - [`enroll`]: first-run exchange of a one-time token for a certificate.
//! - [`credstore`]: the certificate and key, protected at rest.
//! - [`update`] and [`updater`]: signed self-update.
//! - [`telemetry`]: health samples sent on each heartbeat.
//! - [`core`]: the connect/heartbeat/update loop shared by console and service mode.
//! - [`interactive`]: consent, remote input, clipboard and the kill switch.
//! - [`input`]: mouse-coordinate mapping and held-key tracking.
//! - [`device`]: workstation or server (picks the default consent mode).
//! - [`remote`]: remote shell, script runner and file transfer, on streams
//!   the server opens.
//! - [`session`]: when to (re)start the helper in the user's session.
//! - [`paths`]: where the agent keeps its files.
//! - `win` (Windows only): the service, the session helper and its tray icon.

pub mod core;
pub mod credstore;
pub mod device;
pub mod enroll;
pub mod input;
pub mod interactive;
pub mod media;
pub mod paths;
pub mod remote;
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
use protocol::consent::{decide, Decision, DeviceKind, Outcome, SessionRequest};
use protocol::media::VideoPayload;
use protocol::{close_code, read_frame, write_frame, FrameError, Message, PROTOCOL_VERSION};
use quinn::rustls::pki_types::CertificateDer;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::interactive::{DesktopCommand, DesktopEvent, DesktopLink, Sessions};
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
    /// The user's desktop, for consent prompts, toasts, input and clipboard
    /// (Windows: the session helper). Without one, no user is considered
    /// present and input is dropped.
    pub desktop: Option<Arc<DesktopLink>>,
    /// Reported to the server; picks the default consent mode.
    pub device_kind: DeviceKind,
    /// Reported to the server for display, if known.
    pub hostname: Option<String>,
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
    desktop: Option<Arc<DesktopLink>>,
    device_kind: DeviceKind,
    hostname: Option<String>,
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
        desktop: config.desktop.clone(),
        device_kind: config.device_kind,
        hostname: config.hostname.clone(),
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
    /// serving the server's streaming commands if a media source is attached,
    /// its session requests (consent, input, clipboard) through the desktop
    /// link, and the streams it opens for remote operations (shell, scripts,
    /// file transfer).
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
        let _ = outbox_tx.send(Message::DeviceInfo {
            kind: self.device_kind,
        });
        if let Some(hostname) = &self.hostname {
            let _ = outbox_tx.send(Message::AgentInfo {
                hostname: hostname.clone(),
            });
        }
        if let Some(media) = &self.media {
            // Fresh connection, fresh start: any stream from a previous
            // connection has no viewers any more.
            let _ = media.commands.send(MediaCommand::Stop);
        }
        let sessions = Arc::new(std::sync::Mutex::new(Sessions::default()));
        publish_technicians(self.desktop.as_deref(), &sessions);
        // Consent prompts run concurrently; dropped (aborted) with the session.
        let mut consent_tasks = tokio::task::JoinSet::new();

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
                    Message::SessionRequest(request) => {
                        info!(
                            session_id = request.session_id,
                            technician = %request.technician,
                            mode = ?request.mode,
                            "session requested"
                        );
                        lock(&sessions).requested(request.session_id);
                        while consent_tasks.try_join_next().is_some() {}
                        consent_tasks.spawn(consent(
                            request,
                            self.desktop.clone(),
                            sessions.clone(),
                            outbox_tx.clone(),
                        ));
                        continue;
                    }
                    Message::SessionEnded { session_id } => {
                        let ended = lock(&sessions).ended(session_id);
                        info!(session_id, "session ended");
                        if let Some(desktop) = &self.desktop {
                            for event in ended.releases {
                                let _ = desktop.commands.send(DesktopCommand::Input(event));
                            }
                            if ended.was_pending {
                                let _ = desktop.commands.send(DesktopCommand::CancelPrompt {
                                    request_id: session_id,
                                });
                            }
                        }
                        publish_technicians(self.desktop.as_deref(), &sessions);
                        continue;
                    }
                    Message::SessionInput { session_id, event } => {
                        let event = lock(&sessions).input(session_id, event);
                        match (event, &self.desktop) {
                            (Some(event), Some(desktop)) => {
                                let _ = desktop.commands.send(DesktopCommand::Input(event));
                            }
                            (None, _) => {
                                debug!(session_id, "input for an inactive session dropped")
                            }
                            (Some(_), None) => {}
                        }
                        continue;
                    }
                    Message::SessionClipboard { session_id, data } => {
                        let active = lock(&sessions).is_active(session_id);
                        if let (true, Some(desktop)) = (active, &self.desktop) {
                            let _ = desktop.commands.send(DesktopCommand::SetClipboard(data));
                        }
                        continue;
                    }
                    Message::ListMonitors => MediaCommand::ListMonitors,
                    Message::StartStream { monitor } => {
                        // Only stream for a session this agent granted.
                        if !lock(&sessions).any_active() {
                            warn!(monitor, "stream requested with no active session; ignored");
                            continue;
                        }
                        MediaCommand::Start { monitor }
                    }
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

        // The user's clipboard, and the Ctrl+F12 kill switch.
        let desktop_events = async {
            let Some(desktop) = &self.desktop else {
                return std::future::pending::<Result<(), AgentError>>().await;
            };
            let mut source = desktop.events.lock().await;
            while let Some(event) = source.recv().await {
                match event {
                    DesktopEvent::Clipboard(data) => {
                        // Never leak the user's clipboard when nobody is connected.
                        if lock(&sessions).any_active() {
                            let _ = outbox_tx.send(Message::Clipboard(data));
                        }
                    }
                    DesktopEvent::KillSwitch => {
                        let ended = self.end_all_sessions(&sessions);
                        warn!(sessions = ?ended, "user pressed Ctrl+F12: all sessions terminated");
                        if !ended.is_empty() {
                            let _ = outbox_tx.send(Message::UserTerminatedSessions);
                        }
                    }
                }
            }
            std::future::pending().await
        };

        // Shell, script and file-transfer streams opened by the server. The
        // tasks are aborted with the connection, which kills any shell or
        // script still running and discards partial uploads.
        let remote_ops = async {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (mut send, mut recv) = self.connection.accept_bi().await?;
                while tasks.try_join_next().is_some() {}
                tasks.spawn(async move {
                    if remote::serve(&mut send, &mut recv).await.is_ok() {
                        let _ = send.finish();
                    }
                });
            }
        };

        // Each branch only finishes on error. read_frame is not cancel-safe,
        // which is fine: the losers are dropped along with the session.
        let result: Result<(), AgentError> = tokio::select! {
            res = writer => res,
            res = heartbeats => res,
            res = reader => res,
            res = pump => res,
            res = desktop_events => res,
            res = remote_ops => res,
        };
        // Sessions do not survive the connection.
        self.end_all_sessions(&sessions);
        if let Err(AgentError::Unexpected(msg)) = &result {
            warn!(?msg, "closing connection after protocol violation");
            self.connection
                .close(close_code::PROTOCOL_ERROR.into(), b"unexpected message");
        }
        result
    }

    /// End every session locally: release held input, withdraw prompts,
    /// clear the tray list and stop capturing. Returns the ids of the
    /// sessions (active or pending) that were ended.
    fn end_all_sessions(&self, sessions: &std::sync::Mutex<Sessions>) -> Vec<u64> {
        let (mut ended, cancelled, releases) = lock(sessions).terminate_all();
        if let Some(desktop) = &self.desktop {
            for event in releases {
                let _ = desktop.commands.send(DesktopCommand::Input(event));
            }
            for &request_id in &cancelled {
                let _ = desktop
                    .commands
                    .send(DesktopCommand::CancelPrompt { request_id });
            }
        }
        publish_technicians(self.desktop.as_deref(), sessions);
        if let Some(media) = &self.media {
            let _ = media.commands.send(MediaCommand::Stop);
        }
        ended.extend(cancelled);
        ended
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

fn lock(sessions: &std::sync::Mutex<Sessions>) -> std::sync::MutexGuard<'_, Sessions> {
    sessions.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tell the desktop who is connected (for the tray).
fn publish_technicians(desktop: Option<&DesktopLink>, sessions: &std::sync::Mutex<Sessions>) {
    if let Some(desktop) = desktop {
        let list = lock(sessions).technicians();
        let _ = desktop.commands.send(DesktopCommand::Technicians(list));
    }
}

/// Apply the consent policy to one request and report the outcome.
async fn consent(
    request: SessionRequest,
    desktop: Option<Arc<DesktopLink>>,
    sessions: Arc<std::sync::Mutex<Sessions>>,
    outbox: UnboundedSender<Message>,
) {
    let user_present = desktop.as_ref().is_some_and(|d| (d.user_present)());
    let outcome = match decide(request.mode, user_present, request.on_no_user) {
        Decision::Proceed(outcome) | Decision::Refuse(outcome) => outcome,
        Decision::Ask => match &desktop {
            Some(desktop) => {
                let timeout = Duration::from_secs(request.timeout_secs.into());
                desktop
                    .prompt(request.session_id, &request.technician, timeout)
                    .await
                    .outcome()
            }
            None => Outcome::ConsentUnavailable,
        },
    };
    let started = lock(&sessions).resolved(
        request.session_id,
        &request.technician,
        request.mode,
        outcome,
    );
    info!(
        session_id = request.session_id,
        technician = %request.technician,
        mode = ?request.mode,
        user_present,
        %outcome,
        started,
        "consent decided"
    );
    if started {
        publish_technicians(desktop.as_deref(), &sessions);
        if let (Outcome::Notify, Some(desktop)) = (outcome, &desktop) {
            let _ = desktop.commands.send(DesktopCommand::Toast {
                technician: request.technician.clone(),
            });
        }
    }
    let _ = outbox.send(Message::SessionDecision {
        session_id: request.session_id,
        outcome,
    });
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
