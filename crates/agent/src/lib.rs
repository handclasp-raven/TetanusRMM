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

use std::net::SocketAddr;
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
use transport::{Connection, ConnectionError, Preference, TransportKind, TransportSettings};

use crate::interactive::{DesktopCommand, DesktopEvent, DesktopLink, Sessions};
use crate::media::rate::RateController;
use crate::media::source::{MediaCommand, MediaEvent, MediaLink};
use crate::telemetry::TelemetrySource;

pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

pub struct AgentConfig {
    /// Server QUIC (UDP) address.
    pub server_addr: SocketAddr,
    /// Which transports to use, and where the WebSocket fallback is.
    pub transport: TransportSettings,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    pub agent_id: String,
    /// CA(s) the server certificate must chain to.
    pub server_ca: Vec<CertificateDer<'static>>,
    /// Client certificate presented to the server.
    pub identity: Identity,
    pub heartbeat_interval: Duration,
    /// Local UDP address to bind for QUIC. `None` binds the unspecified
    /// address of the server's address family, which is what production
    /// agents should use (see [`AgentSession::rebind`] for why).
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
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
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
    /// The QUIC endpoint; `None` on the WebSocket fallback.
    endpoint: Option<quinn::Endpoint>,
    connection: Connection,
    agent_id: String,
    heartbeat_interval: Duration,
    telemetry: Option<Arc<dyn TelemetrySource>>,
    media: Option<Arc<MediaLink>>,
    desktop: Option<Arc<DesktopLink>>,
    device_kind: DeviceKind,
    hostname: Option<String>,
}

/// Connect to the server and complete the mutual-TLS handshake, on the
/// transport `config.transport` allows (QUIC, else the WebSocket fallback).
pub async fn connect(config: &AgentConfig) -> Result<AgentSession, AgentError> {
    connect_with(config, &mut Preference::default()).await
}

impl From<transport::dial::DialError> for AgentError {
    fn from(e: transport::dial::DialError) -> Self {
        match e {
            transport::dial::DialError::Tls(e) => AgentError::Tls(e),
            other => AgentError::Unreachable(other.to_string()),
        }
    }
}

/// [`connect`], remembering across reconnects which transport worked (see
/// [`transport::dial`]).
pub async fn connect_with(
    config: &AgentConfig,
    preference: &mut Preference,
) -> Result<AgentSession, AgentError> {
    let target = config
        .transport
        .target(config.server_addr, &config.server_name, config.bind_addr);
    let dialer = transport::Dialer::new(target, &config.server_ca, Some(&config.identity))?;
    let dialed = dialer.dial(preference).await?;
    info!(
        server = %dialed.connection.remote_address(),
        transport = %dialed.connection.kind(),
        "connected"
    );
    Ok(AgentSession {
        endpoint: dialed.endpoint,
        connection: dialed.connection,
        agent_id: config.agent_id.clone(),
        heartbeat_interval: config.heartbeat_interval,
        telemetry: config.telemetry.clone(),
        media: config.media.clone(),
        desktop: config.desktop.clone(),
        device_kind: config.device_kind,
        hostname: config.hostname.clone(),
    })
}

fn quic_only() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "not a QUIC connection (on the WebSocket fallback)",
    )
}

impl AgentSession {
    /// Which transport the session runs on.
    pub fn transport(&self) -> TransportKind {
        self.connection.kind()
    }

    /// Local UDP address (QUIC only).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.as_ref().ok_or_else(quic_only)?.local_addr()
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
    ///
    /// QUIC only: the WebSocket fallback runs on TCP, which cannot migrate;
    /// a network change there drops the connection and the agent reconnects.
    pub fn rebind(&self, socket: std::net::UdpSocket) -> std::io::Result<()> {
        self.endpoint.as_ref().ok_or_else(quic_only)?.rebind(socket)
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
        // Adaptive bitrate: fed by the video pump and the server's reports.
        let rate = std::sync::Mutex::new(RateController::new());
        let set_bitrate = |change: Option<crate::media::rate::Change>| {
            let (Some(change), Some(media)) = (change, &self.media) else {
                return;
            };
            info!(
                bps = change.bps,
                reason = ?change.reason,
                uplink_delay_ms = change.uplink_delay.as_millis() as u64,
                viewer_delay_ms = change.viewer_delay.as_millis() as u64,
                "bitrate adapted"
            );
            let command = MediaCommand::SetBitrate(change.bps);
            if let Some(tx) = &events {
                let _ = tx.send(AgentEvent::Media(command.clone()));
            }
            let _ = media.commands.send(command);
        };
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
                    Message::StreamReport(report) => {
                        debug!(?report, "stream report");
                        let change = lock(&rate).on_report(&report, std::time::Instant::now());
                        set_bitrate(change);
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
                    Message::StopStream => {
                        lock(&rate).stream_restarted();
                        MediaCommand::Stop
                    }
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
            let mut video: Option<transport::SendStream> = None;
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
                        // Timed from before the write: time blocked on a
                        // saturated transport is queueing too.
                        let change = lock(&rate).on_sent(
                            seq,
                            payload.width,
                            payload.height,
                            std::time::Instant::now(),
                        );
                        set_bitrate(change);
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
                        lock(&rate).stream_restarted();
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
                .close(close_code::PROTOCOL_ERROR, b"unexpected message");
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
    pub fn close_reason(&self) -> Option<ConnectionError> {
        self.connection.close_reason()
    }

    /// Close the connection cleanly.
    pub fn close(&self) {
        self.connection
            .close(close_code::NORMAL, b"agent shutting down");
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
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
