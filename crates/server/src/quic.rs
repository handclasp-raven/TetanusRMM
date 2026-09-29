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
//! | no  | `ViewerHello` | Viewer token consumed; the viewer is subscribed to its agent's video via the relay |
//! | anything else | | Closed with `PROTOCOL_ERROR` or `UNAUTHORIZED` |
//!
//! An agent's connection also carries its video: the agent opens
//! unidirectional streams of `MediaFrame`s, which feed the [`Hub`]'s
//! fan-out. The server writes `StartStream`/`StopStream`/`RequestKeyframe`/
//! `ListMonitors` on the agent's control stream as viewers come and go.
//!
//! A viewer's session starts only after consent: the server sends the
//! agent a `SessionRequest` with the device's policy, waits for its
//! `SessionDecision`, and audits the mode and outcome (`session.start`).
//! A refused viewer is closed with `CONSENT_REFUSED` and the reason. Once
//! started, the viewer's input and clipboard are relayed to the agent
//! tagged with the session id, and the agent's clipboard goes to every
//! viewer. The user's Ctrl+F12 (`UserTerminatedSessions`) closes every
//! viewer of that agent with `USER_TERMINATED`, audited per session.
//!
//! Without a registry (tests only), any certificate from the CA may say
//! `Hello` and enrollment is unavailable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::Identity;
use protocol::clipboard::MAX_CLIPBOARD_BYTES;
use protocol::consent::{ConsentMode, OnNoUser, Outcome, SessionRequest};
use protocol::media::MediaFrame;
use protocol::{close_code, read_frame, write_frame, FrameError, Message};
use quinn::rustls::pki_types::CertificateDer;
use ring::digest::{digest, SHA256};
use sqlx::PgPool;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{debug, error, info, info_span, warn, Instrument};

use crate::enroll::{self, AgentCa, EnrollError};
use crate::registry;
use crate::relay::Hub;
use crate::viewers::{self, ViewerError, ViewerGrant};

/// How long to wait for an enrolling agent to read its certificate and hang up.
const ENROLL_LINGER: Duration = Duration::from_secs(10);

/// How long the agent may take to decide consent, beyond the prompt's own
/// timeout, before the server gives up (`consent_unavailable`).
const DECISION_GRACE: Duration = Duration::from_secs(15);

/// Oldest agent protocol that enforces consent. Older agents cannot be viewed.
const MIN_CONSENT_VERSION: u32 = 4;

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
    ViewerConnected {
        conn_id: usize,
        agent_id: String,
        username: String,
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
    #[error("agent is not connected")]
    AgentOffline,
    #[error("consent refused: {0}")]
    ConsentRefused(Outcome),
    #[error("the user ended the session")]
    UserTerminated,
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

pub struct Server {
    endpoint: quinn::Endpoint,
    events: Option<UnboundedSender<ServerEvent>>,
    registry: Option<Arc<Registry>>,
    hub: Arc<Hub>,
}

/// Per-connection context shared with the connection task.
#[derive(Clone)]
struct Hooks {
    events: Option<UnboundedSender<ServerEvent>>,
    registry: Option<Arc<Registry>>,
    hub: Arc<Hub>,
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
            hub: Hub::new(),
        })
    }

    /// The media relay (shared with the HTTPS API).
    pub fn hub(&self) -> Arc<Hub> {
        self.hub.clone()
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
                hub: self.hub.clone(),
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
        Err(ConnError::AgentOffline) => {
            conn.close(close_code::AGENT_OFFLINE.into(), b"agent is not connected");
        }
        Err(ConnError::ConsentRefused(outcome)) => {
            conn.close(
                close_code::CONSENT_REFUSED.into(),
                outcome.refusal_reason().as_bytes(),
            );
        }
        Err(ConnError::UserTerminated) => {
            conn.close(
                close_code::USER_TERMINATED.into(),
                Outcome::UserTerminatedSession.refusal_reason().as_bytes(),
            );
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
            serve_agent(conn, conn_id, &agent_id, version, send, recv, hooks).await
        }
        (Some(Message::ViewerHello { token, version }), None) => {
            let registry = hooks
                .registry
                .as_ref()
                .ok_or(ConnError::Unauthorized("viewing is not available"))?;
            let grant = match viewers::connect(&registry.pool, &token).await {
                Ok(grant) => grant,
                Err(ViewerError::Db(e)) => return Err(e.into()),
                Err(_) => return Err(ConnError::Unauthorized("invalid viewer token")),
            };
            info!(conn_id, agent_id = %grant.agent_id, user = %grant.username, version, "viewer connected");
            hooks.emit(ServerEvent::ViewerConnected {
                conn_id,
                agent_id: grant.agent_id.clone(),
                username: grant.username.clone(),
            });
            let mut frames_sent = 0;
            let result = serve_viewer(
                conn,
                &registry.pool,
                &grant,
                send,
                recv,
                hooks,
                &mut frames_sent,
            )
            .await;
            if let Err(e) = viewers::end(&registry.pool, &grant, frames_sent).await {
                warn!(conn_id, "recording viewer disconnect failed: {e}");
            }
            info!(conn_id, frames_sent, "viewer disconnected");
            result
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
        (Some(_), _) => Err(ConnError::Protocol("expected Hello, Enroll or ViewerHello")),
    }
}

/// An authenticated agent: heartbeats, monitor lists and video, plus
/// commands from the relay written back on the control stream.
async fn serve_agent(
    conn: &quinn::Connection,
    conn_id: usize,
    agent_id: &str,
    version: u32,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    hooks: &Hooks,
) -> Result<(), ConnError> {
    let (to_agent, mut outbox) = mpsc::unbounded_channel::<Message>();
    let link = hooks.hub.register(agent_id, version, to_agent.clone());
    // Agents older than protocol 3 do not know the streaming messages.
    if version >= 3 {
        let _ = to_agent.send(Message::ListMonitors);
    }

    let writer = async {
        while let Some(msg) = outbox.recv().await {
            write_frame(&mut send, &msg).await?;
        }
        Ok::<(), ConnError>(())
    };

    let reader = async {
        while let Some(msg) = read_frame(&mut recv).await? {
            match msg {
                Message::Heartbeat { ts, seq, telemetry } => {
                    let remote = conn.remote_address();
                    debug!(%agent_id, seq, ts, %remote, ?telemetry, "heartbeat");
                    hooks.emit(ServerEvent::Heartbeat {
                        conn_id,
                        agent_id: agent_id.to_owned(),
                        seq,
                        remote,
                    });
                    if let Some(registry) = &hooks.registry {
                        // A registry hiccup should not disconnect an authenticated agent.
                        if let Err(e) =
                            registry::touch(&registry.pool, agent_id, telemetry.as_ref()).await
                        {
                            warn!(%agent_id, "registry update failed: {e}");
                        }
                    }
                    let _ = to_agent.send(Message::HeartbeatAck { seq });
                }
                Message::MonitorList { monitors } => {
                    debug!(%agent_id, count = monitors.len(), "monitor list");
                    link.set_monitors(monitors);
                }
                Message::DeviceInfo { kind } => {
                    info!(%agent_id, ?kind, "device info");
                    if let Some(registry) = &hooks.registry {
                        match registry::record_device_kind(&registry.pool, agent_id, kind).await {
                            Ok(Some(mode)) => {
                                info!(%agent_id, ?mode, "default consent mode applied")
                            }
                            Ok(None) => {}
                            Err(e) => warn!(%agent_id, "recording device kind failed: {e}"),
                        }
                    }
                }
                Message::SessionDecision {
                    session_id,
                    outcome,
                } => {
                    info!(%agent_id, session_id, %outcome, "consent decided");
                    link.on_decision(session_id, outcome);
                }
                Message::Clipboard(data) => {
                    if data.size() <= MAX_CLIPBOARD_BYTES {
                        link.on_clipboard(data);
                    }
                }
                Message::UserTerminatedSessions => {
                    warn!(%agent_id, viewers = link.viewers(), "user pressed Ctrl+F12");
                    link.user_terminated();
                }
                other => {
                    warn!(%agent_id, ?other, "unexpected message on control stream");
                    return Err(ConnError::Protocol("unexpected message"));
                }
            }
        }
        Ok(())
    };

    // Video: each unidirectional stream from the agent is a sequence of frames.
    let media = async {
        loop {
            let mut stream = conn.accept_uni().await?;
            while let Some(frame) = read_frame::<_, MediaFrame>(&mut stream).await? {
                link.on_frame(frame);
            }
            link.stream_reset();
        }
    };

    let result = tokio::select! {
        r = writer => r,
        r = reader => r,
        r = media => r,
    };
    hooks.hub.unregister(&link);
    result
}

/// A viewer session with `grant.agent_id`, through the relay: consent
/// first, then video out and input/clipboard in.
async fn serve_viewer(
    conn: &quinn::Connection,
    pool: &PgPool,
    grant: &ViewerGrant,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    hooks: &Hooks,
    frames_sent: &mut u64,
) -> Result<(), ConnError> {
    let link = hooks
        .hub
        .get(&grant.agent_id)
        .ok_or(ConnError::AgentOffline)?;
    let session_id = grant.session_id as u64;
    let mut agent_closed = link.closed();
    let mut terminations = link.terminations();
    terminations.mark_unchanged();

    // --- Consent --------------------------------------------------------
    let policy = registry::get_policy(pool, &grant.agent_id).await?;
    let (mode, on_no_user, timeout_secs) = match policy {
        Some(p) => (
            ConsentMode::from(p.consent_mode),
            OnNoUser::from(p.on_no_user),
            p.consent_timeout_secs.max(1) as u32,
        ),
        None => (ConsentMode::Notify, OnNoUser::Deny, 30),
    };
    if link.version < MIN_CONSENT_VERSION {
        // It would ignore the request: refuse rather than skip consent.
        warn!(agent_id = %grant.agent_id, version = link.version, "agent too old to enforce consent");
        viewers::record_consent(pool, grant, mode, Outcome::ConsentUnavailable).await?;
        return Err(ConnError::ConsentRefused(Outcome::ConsentUnavailable));
    }
    // From here on, however this ends, the agent hears that it did.
    let _ended = EndSession {
        link: &link,
        session_id,
    };
    if mode == ConsentMode::Require {
        write_frame(&mut send, &Message::ConsentPending { timeout_secs }).await?;
    }
    let decision = link.request_session(SessionRequest {
        session_id,
        technician: grant.username.clone(),
        mode,
        on_no_user,
        timeout_secs,
    });
    let wait = Duration::from_secs(timeout_secs.into()) + DECISION_GRACE;
    let outcome = tokio::select! {
        decided = tokio::time::timeout(wait, decision) => match decided {
            Ok(Ok(outcome)) => outcome,
            // No answer from the agent: nobody approved it.
            _ => Outcome::ConsentUnavailable,
        },
        // (The watch guard is dropped at once: it must not live across an await.)
        _ = async { agent_closed.wait_for(|closed| *closed).await.map(drop) } => {
            return Err(ConnError::AgentOffline)
        }
        _ = terminations.changed() => {
            viewers::record_user_terminated(pool, grant).await?;
            return Err(ConnError::UserTerminated);
        }
        _ = conn.closed() => {
            info!(session_id, "viewer left while consent was pending");
            return Ok(());
        }
    };
    viewers::record_consent(pool, grant, mode, outcome).await?;
    if !outcome.allows_session() {
        info!(session_id, %outcome, "session refused");
        return Err(ConnError::ConsentRefused(outcome));
    }
    info!(session_id, %outcome, "session started");

    // --- Session ---------------------------------------------------------
    let viewer_id = hooks.hub.next_viewer_id();
    let mut monitors = link.monitors();
    let mut stream_monitor = link.stream_monitor();
    let mut clipboard = link.clipboard();

    let (to_viewer, mut outbox) = mpsc::unbounded_channel::<Message>();
    let _ = to_viewer.send(Message::ViewerWelcome {
        agent_id: grant.agent_id.clone(),
        monitors: monitors.borrow_and_update().clone(),
        active_monitor: *stream_monitor.borrow(),
    });
    let mut subscription = link.subscribe(viewer_id);
    let mut video = conn.open_uni().await?;

    let writer = async {
        while let Some(msg) = outbox.recv().await {
            write_frame(&mut send, &msg).await?;
        }
        Ok::<(), ConnError>(())
    };
    let forward = async {
        while let Some(frame) = subscription.frames.recv().await {
            write_frame(&mut video, frame.as_ref()).await?;
            *frames_sent += 1;
        }
        Ok(())
    };
    let notify = async {
        loop {
            tokio::select! {
                changed = monitors.changed() => {
                    if changed.is_err() { break; }
                    let list = monitors.borrow_and_update().clone();
                    let _ = to_viewer.send(Message::MonitorList { monitors: list });
                }
                changed = stream_monitor.changed() => {
                    if changed.is_err() { break; }
                    if let Some(monitor) = *stream_monitor.borrow_and_update() {
                        let _ = to_viewer.send(Message::StreamMonitor { monitor });
                    }
                }
                data = clipboard.recv() => match data {
                    Ok(data) => { let _ = to_viewer.send(Message::Clipboard(data)); }
                    // Missed some: only the latest matters, and it comes next.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = terminations.changed() => {
                    viewers::record_user_terminated(pool, grant).await?;
                    return Err(ConnError::UserTerminated);
                }
                _ = async { agent_closed.wait_for(|closed| *closed).await.map(drop) } => break,
            }
        }
        Err::<(), _>(ConnError::AgentOffline)
    };
    let reader = async {
        while let Some(msg) = read_frame(&mut recv).await? {
            match msg {
                Message::SelectMonitor { monitor } => link.select_monitor(monitor),
                Message::RequestKeyframe => link.viewer_requests_keyframe(viewer_id),
                Message::Input(event) => link.send(Message::SessionInput { session_id, event }),
                Message::Clipboard(data) if data.size() <= MAX_CLIPBOARD_BYTES => {
                    link.send(Message::SessionClipboard { session_id, data })
                }
                Message::Clipboard(data) => {
                    warn!(
                        bytes = data.size(),
                        "oversized clipboard from viewer dropped"
                    )
                }
                other => {
                    warn!(?other, "unexpected message from viewer");
                    return Err(ConnError::Protocol("unexpected message"));
                }
            }
        }
        Ok(())
    };

    tokio::select! {
        r = writer => r,
        r = forward => r,
        r = notify => r,
        r = reader => r,
    }
}

/// Tells the agent a session is over when dropped.
struct EndSession<'a> {
    link: &'a crate::relay::AgentLink,
    session_id: u64,
}

impl Drop for EndSession<'_> {
    fn drop(&mut self) {
        self.link.end_session(self.session_id);
    }
}

/// Hex SHA-256 of a DER certificate.
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> String {
    hex::encode(digest(&SHA256, cert.as_ref()))
}
