//! Listeners for agents and viewers: QUIC, and the WebSocket-over-TLS
//! fallback for networks that block UDP (see the `transport` crate). Both
//! carry the same streams and are served by the same code below; only
//! accepting the connection differs.
//!
//! Each connection gets its own task and opens one bidirectional control
//! stream. What is allowed on it depends on whether the agent presented a
//! client certificate during the handshake:
//!
//! | Client cert | First message | Outcome |
//! |---|---|---|
//! | yes | `Hello` | Accepted if the cert is pinned to that agent id in the registry, then `Heartbeat`/`HeartbeatAck` |
//! | no  | `Enroll` | Token consumed, certificate issued, `Enrolled` sent, connection ends |
//! | no  | `AssistEnroll` | Quick assist code consumed (see `crate::assist`), short-lived certificate issued, `Enrolled` sent, connection ends |
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
//! A refused viewer is closed with `CONSENT_REFUSED` and the reason. The
//! user's Ctrl+F12 (`UserTerminatedSessions`) closes every viewer of that
//! agent with `USER_TERMINATED`, audited per session.
//!
//! A user can lend the technicians a password for the length of a session
//! (see `protocol::credential`). It never leaves the agent; the agent
//! reports what happens to it (`CredentialEvent`), audited as
//! `credential.*`.
//!
//! Sessions are end-to-end encrypted between agent and viewer (see
//! `protocol::e2e`). The viewer sends its session key right after
//! `ViewerHello`; the server hands it to the agent with the session request
//! (`SessionViewerKey`), which is how the agent knows who it is talking
//! to. After that the server only relays: `Sealed` records between the
//! agent and that session's viewer, and sealed video. It never sees input,
//! clipboard, screen content, or the signaling for a direct path. Both
//! ends must speak protocol 8; older viewers and agents are refused
//! (`OUTDATED`) rather than served in the clear.
//!
//! When a session moves to a direct path, the viewer says so
//! (`PathReport`): the relay stops sending it video, and the move is
//! counted in metrics and the audit log (`session.path`).
//!
//! Without a registry (tests only), any certificate from the CA may say
//! `Hello` and enrollment is unavailable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::Identity;
use protocol::consent::{ConsentMode, OnNoUser, Outcome, SessionRequest};
use protocol::e2e::{Path, KEY_LEN, MAX_SEALED_LEN};
use protocol::media::MediaFrame;
use protocol::{close_code, read_frame, write_frame, FrameError, Message};
use quinn::rustls::pki_types::CertificateDer;
use ring::digest::{digest, SHA256};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{debug, error, info, info_span, warn, Instrument};
use transport::{Connection, ConnectionError, TransportKind};

use crate::assist::{self, AssistError};
use crate::enroll::{self, AgentCa, EnrollError};
use crate::metrics::{self, ConnectedGuard, PathGauge, ReasonLabels, ResultLabels, SessionLabels};
use crate::registry;
use crate::relay::Hub;
use crate::viewers::{self, ViewerError, ViewerGrant};

/// How long to wait for an enrolling agent to read its certificate and hang up.
const ENROLL_LINGER: Duration = Duration::from_secs(10);

/// How long the agent may take to decide consent, beyond the prompt's own
/// timeout, before the server gives up (`consent_unavailable`).
const DECISION_GRACE: Duration = Duration::from_secs(15);

/// What agents and viewers are told about direct paths (`PeerConfig`).
#[derive(Debug, Clone, Copy)]
struct PeerPolicy {
    direct: bool,
    stun_port: Option<u16>,
}

impl PeerPolicy {
    fn message(self) -> Message {
        Message::PeerConfig {
            direct: self.direct,
            stun_port: self.stun_port,
        }
    }
}

pub struct ServerConfig {
    /// UDP address for QUIC.
    pub listen: SocketAddr,
    /// TCP address for the WebSocket fallback (`None`: QUIC only).
    pub ws_listen: Option<SocketAddr>,
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
        transport: TransportKind,
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
    #[error("binding WebSocket listener on {addr}: {source}")]
    BindWs {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
enum ConnError {
    #[error(transparent)]
    Connection(#[from] ConnectionError),
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
    #[error("outdated: {0}")]
    Outdated(&'static str),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

pub struct Server {
    endpoint: quinn::Endpoint,
    /// The WebSocket fallback listener and its TLS acceptor.
    ws: Option<(TcpListener, tokio_rustls::TlsAcceptor)>,
    events: Option<UnboundedSender<ServerEvent>>,
    registry: Option<Arc<Registry>>,
    hub: Arc<Hub>,
    peer: PeerPolicy,
    assist: Arc<assist::Limiter>,
}

/// Per-connection context shared with the connection task.
#[derive(Clone)]
struct Hooks {
    events: Option<UnboundedSender<ServerEvent>>,
    registry: Option<Arc<Registry>>,
    hub: Arc<Hub>,
    peer: PeerPolicy,
    /// Limits wrong quick assist codes.
    assist: Arc<assist::Limiter>,
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
        let ws = match config.ws_listen {
            Some(addr) => {
                let tls = common::quic::mutual_tls_server(
                    &config.identity,
                    &config.client_ca,
                    transport::ws::ALPN,
                )?;
                let bind_err = |source| ServerError::BindWs { addr, source };
                let listener = std::net::TcpListener::bind(addr).map_err(bind_err)?;
                listener.set_nonblocking(true).map_err(bind_err)?;
                let listener = TcpListener::from_std(listener).map_err(bind_err)?;
                Some((listener, transport::ws::acceptor(tls)))
            }
            None => None,
        };
        Ok(Self {
            endpoint,
            ws,
            events: None,
            registry: None,
            hub: Hub::new(),
            peer: PeerPolicy {
                direct: true,
                stun_port: None,
            },
            assist: Arc::default(),
        })
    }

    /// Whether sessions may try direct paths (default: yes), and the port
    /// of the STUN responder on this server's address, if one runs
    /// (default: none, so peers offer only their local addresses).
    pub fn with_direct_paths(mut self, direct: bool, stun_port: Option<u16>) -> Self {
        self.peer = PeerPolicy { direct, stun_port };
        self
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

    /// The QUIC (UDP) address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// The WebSocket fallback's (TCP) address, if it is enabled.
    pub fn ws_local_addr(&self) -> Option<SocketAddr> {
        self.ws.as_ref()?.0.local_addr().ok()
    }

    fn hooks(&self) -> Hooks {
        Hooks {
            events: self.events.clone(),
            registry: self.registry.clone(),
            hub: self.hub.clone(),
            peer: self.peer,
            assist: self.assist.clone(),
        }
    }

    /// Accept connections until the QUIC endpoint is closed.
    pub async fn run(&self) {
        tokio::select! {
            () = self.run_quic() => {}
            () = self.run_ws() => {}
        }
    }

    async fn run_quic(&self) {
        info!(addr = ?self.endpoint.local_addr().ok(), "listening for agents (QUIC)");
        while let Some(incoming) = self.endpoint.accept().await {
            let span =
                info_span!("agent_conn", remote = %incoming.remote_address(), transport = "quic");
            let hooks = self.hooks();
            tokio::spawn(
                async move {
                    let result = match incoming.await {
                        Ok(conn) => {
                            let fingerprint = conn
                                .peer_identity()
                                .and_then(|id| id.downcast::<Vec<CertificateDer<'static>>>().ok())
                                .and_then(|chain| chain.first().map(|leaf| cert_fingerprint(leaf)));
                            handle_connection(Connection::Quic(conn), fingerprint, hooks).await
                        }
                        // The handshake fails here if the client presented a
                        // certificate that does not chain to the client CA.
                        Err(e) => Err(ConnError::Connection(e.into())),
                    };
                    log_end(result);
                }
                .instrument(span),
            );
        }
    }

    /// The WebSocket fallback: TLS (same certificates and client-cert
    /// policy as QUIC), then the upgrade, then the same handling.
    async fn run_ws(&self) {
        let Some((listener, acceptor)) = &self.ws else {
            return std::future::pending().await;
        };
        info!(addr = ?listener.local_addr().ok(), "listening for agents (WebSocket fallback)");
        loop {
            let (tcp, remote) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    // Out of file descriptors and the like: back off, carry on.
                    warn!("accepting WebSocket connection: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let span = info_span!("agent_conn", %remote, transport = "websocket");
            let acceptor = acceptor.clone();
            let hooks = self.hooks();
            tokio::spawn(
                async move {
                    let accepted = match transport::ws::accept_tls(&acceptor, tcp, remote).await {
                        Ok(accepted) => accepted,
                        Err(e) => return info!("TLS handshake failed: {e}"),
                    };
                    let fingerprint = accepted
                        .peer_certificates()
                        .and_then(|chain| chain.first().map(cert_fingerprint));
                    let conn = match transport::ws::upgrade(accepted).await {
                        Ok(conn) => conn,
                        Err(e) => return info!("WebSocket upgrade failed: {e}"),
                    };
                    log_end(
                        handle_connection(Connection::WebSocket(conn), fingerprint, hooks).await,
                    );
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

fn log_end(result: Result<(), ConnError>) {
    match result {
        Ok(()) => info!("disconnected"),
        Err(e) => info!("connection ended: {e}"),
    }
}

async fn handle_connection(
    conn: Connection,
    fingerprint: Option<String>,
    hooks: Hooks,
) -> Result<(), ConnError> {
    let conn_id = conn.stable_id();
    match &fingerprint {
        Some(fp) => info!(conn_id, fingerprint = %fp, "client certificate verified"),
        None => info!(conn_id, "connection without client certificate"),
    }

    let result = dispatch(&conn, conn_id, fingerprint.as_deref(), &hooks).await;
    let rejected = match &result {
        Err(ConnError::Protocol(_)) => Some("protocol_error"),
        Err(ConnError::Unauthorized(_)) => Some("unauthorized"),
        Err(ConnError::AgentOffline) => Some("agent_offline"),
        Err(ConnError::ConsentRefused(_)) => Some("consent_refused"),
        Err(ConnError::UserTerminated) => Some("user_terminated"),
        Err(ConnError::Outdated(_)) => Some("outdated"),
        Err(ConnError::Db(_)) => Some("server_error"),
        _ => None,
    };
    if let Some(reason) = rejected {
        metrics::get()
            .connections_rejected
            .get_or_create(&ReasonLabels { reason })
            .inc();
    }
    match &result {
        Err(ConnError::Protocol(reason)) => {
            conn.close(close_code::PROTOCOL_ERROR, reason.as_bytes());
        }
        Err(ConnError::Unauthorized(reason)) => {
            warn!(conn_id, "rejected: {reason}");
            conn.close(close_code::UNAUTHORIZED, reason.as_bytes());
        }
        Err(ConnError::AgentOffline) => {
            conn.close(close_code::AGENT_OFFLINE, b"agent is not connected");
        }
        Err(ConnError::ConsentRefused(outcome)) => {
            conn.close(
                close_code::CONSENT_REFUSED,
                outcome.refusal_reason().as_bytes(),
            );
        }
        Err(ConnError::UserTerminated) => {
            conn.close(
                close_code::USER_TERMINATED,
                Outcome::UserTerminatedSession.refusal_reason().as_bytes(),
            );
        }
        Err(ConnError::Outdated(reason)) => {
            conn.close(close_code::OUTDATED, reason.as_bytes());
        }
        Err(ConnError::Db(e)) => {
            error!(conn_id, "database error: {e}");
            conn.close(close_code::PROTOCOL_ERROR, b"server error");
        }
        _ => {}
    }
    result
}

async fn dispatch(
    conn: &Connection,
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
                transport: conn.kind(),
            });
            serve_agent(conn, conn_id, &agent_id, version, send, recv, hooks).await
        }
        (Some(Message::ViewerHello { token, version }), None) => {
            let registry = hooks
                .registry
                .as_ref()
                .ok_or(ConnError::Unauthorized("viewing is not available"))?;
            // Checked before the token is spent, so an updated viewer can
            // still use it.
            if version < protocol::MIN_E2E_VERSION {
                return Err(ConnError::Outdated(
                    "this viewer is too old: sessions are end-to-end encrypted; update it",
                ));
            }
            let key = match read_frame(&mut recv).await? {
                Some(Message::ViewerKey { public }) => public,
                _ => return Err(ConnError::Protocol("expected ViewerKey after ViewerHello")),
            };
            let grant = match viewers::connect(&registry.pool, &token).await {
                Ok(grant) => grant,
                Err(ViewerError::Db(e)) => return Err(e.into()),
                Err(ViewerError::Forbidden) => {
                    return Err(ConnError::Unauthorized("not allowed to view this agent"))
                }
                Err(_) => return Err(ConnError::Unauthorized("invalid viewer token")),
            };
            info!(conn_id, agent_id = %grant.agent_id, user = %grant.username, version, "viewer connected");
            hooks.emit(ServerEvent::ViewerConnected {
                conn_id,
                agent_id: grant.agent_id.clone(),
                username: grant.username.clone(),
            });
            let mut ended = Ended {
                frames_sent: 0,
                path: Path::Relayed,
            };
            let viewer = ViewerConn {
                conn,
                version,
                key,
                send,
                recv,
            };
            let result = serve_viewer(viewer, &registry.pool, &grant, hooks, &mut ended).await;
            if let Err(e) =
                viewers::end(&registry.pool, &grant, ended.frames_sent, ended.path).await
            {
                warn!(conn_id, "recording viewer disconnect failed: {e}");
            }
            info!(conn_id, frames_sent = ended.frames_sent, path = %ended.path, "viewer disconnected");
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
                        enrollment_result("invalid_token");
                        return Err(ConnError::Unauthorized("invalid enrollment token"));
                    }
                    Err(EnrollError::Ca(e)) => {
                        enrollment_result("invalid_csr");
                        warn!(conn_id, "enrollment CSR rejected: {e}");
                        return Err(ConnError::Protocol("invalid certificate signing request"));
                    }
                    Err(EnrollError::Db(e)) => return Err(e.into()),
                };
            info!(conn_id, agent_id = %enrolled.agent_id, fingerprint = %enrolled.fingerprint, "agent enrolled");
            enrollment_result("enrolled");
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
        (Some(Message::AssistEnroll { code, csr_der }), None) => {
            let registry = hooks
                .registry
                .as_ref()
                .ok_or(ConnError::Unauthorized("quick assist is not available"))?;
            let remote = conn.remote_address().ip();
            if !hooks.assist.allows(remote, Instant::now()) {
                enrollment_result("assist_rate_limited");
                return Err(ConnError::Unauthorized(protocol::assist::REJECT_TOO_MANY));
            }
            let enrolled =
                match assist::redeem(&registry.pool, &registry.ca, &code, &csr_der, remote).await {
                    Ok(enrolled) => enrolled,
                    Err(AssistError::InvalidCode) => {
                        enrollment_result("assist_invalid_code");
                        if let assist::Failure::Lockout(failures) =
                            hooks.assist.failed(remote, Instant::now())
                        {
                            let voided = assist::void_all(&registry.pool, failures).await?;
                            warn!(
                                failures,
                                voided,
                                "too many wrong quick assist codes: outstanding codes voided"
                            );
                        }
                        return Err(ConnError::Unauthorized(protocol::assist::REJECT_CODE));
                    }
                    Err(AssistError::Ca(e)) => {
                        enrollment_result("invalid_csr");
                        warn!(conn_id, "quick assist CSR rejected: {e}");
                        return Err(ConnError::Protocol("invalid certificate signing request"));
                    }
                    Err(AssistError::NoFreeCode) => {
                        return Err(ConnError::Protocol("unexpected quick assist error"))
                    }
                    Err(AssistError::Db(e)) => return Err(e.into()),
                };
            info!(conn_id, agent_id = %enrolled.agent_id, fingerprint = %enrolled.fingerprint, "quick assist code redeemed");
            enrollment_result("assist_redeemed");
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
            let _ = tokio::time::timeout(ENROLL_LINGER, conn.closed()).await;
            Ok(())
        }
        (Some(Message::Enroll { .. } | Message::AssistEnroll { .. }), Some(_)) => {
            Err(ConnError::Protocol("already enrolled; send Hello"))
        }
        (Some(_), _) => Err(ConnError::Protocol(
            "expected Hello, Enroll, AssistEnroll or ViewerHello",
        )),
    }
}

/// An authenticated agent: heartbeats, monitor lists and video, plus
/// commands from the relay written back on the control stream.
async fn serve_agent(
    conn: &Connection,
    conn_id: usize,
    agent_id: &str,
    version: u32,
    mut send: transport::SendStream,
    mut recv: transport::RecvStream,
    hooks: &Hooks,
) -> Result<(), ConnError> {
    let _connected = ConnectedGuard::agent(conn.kind());
    let (to_agent, mut outbox) = mpsc::unbounded_channel::<Message>();
    let link = hooks
        .hub
        .register(agent_id, version, to_agent.clone(), Some(conn.clone()));
    // Agents older than protocol 3 do not know the streaming messages.
    if version >= 3 {
        let _ = to_agent.send(Message::ListMonitors);
    }
    if version >= protocol::MIN_E2E_VERSION {
        let _ = to_agent.send(hooks.peer.message());
    }
    if version >= protocol::MIN_STATUS_VERSION {
        let _ = to_agent.send(Message::EnableStatusReports);
    }
    // Only with somewhere to audit it: an agent that is not asked keeps no
    // password.
    if version >= protocol::MIN_CREDENTIAL_VERSION && hooks.registry.is_some() {
        let _ = to_agent.send(Message::EnableCredentialReports);
    }
    // Where the agent connects from; re-recorded when that changes.
    let mut remote_ip = None;

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
                    metrics::get().heartbeats.inc();
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
                        if remote_ip != Some(remote.ip()) {
                            match registry::record_remote_ip(&registry.pool, agent_id, remote.ip())
                                .await
                            {
                                Ok(()) => remote_ip = Some(remote.ip()),
                                Err(e) => warn!(%agent_id, "recording remote address failed: {e}"),
                            }
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
                Message::Sealed { session_id, data } => {
                    if data.len() > MAX_SEALED_LEN {
                        return Err(ConnError::Protocol("oversized sealed record"));
                    }
                    metrics::get().sealed_records.inc();
                    link.on_sealed(session_id, data);
                }
                Message::UserTerminatedSessions => {
                    metrics::get().user_terminations.inc();
                    warn!(%agent_id, viewers = link.viewers(), "user pressed Ctrl+F12");
                    link.user_terminated();
                }
                Message::CredentialEvent { session_id, event } => {
                    info!(%agent_id, ?session_id, %event, "lent password");
                    if let Some(registry) = &hooks.registry {
                        viewers::record_credential(&registry.pool, agent_id, session_id, event)
                            .await?;
                    }
                }
                Message::AgentInfo { hostname } => {
                    let hostname = protocol::sanitize_hostname(&hostname);
                    info!(%agent_id, ?hostname, "agent info");
                    if let (Some(registry), Some(hostname)) = (&hooks.registry, hostname) {
                        if let Err(e) =
                            registry::record_hostname(&registry.pool, agent_id, &hostname).await
                        {
                            warn!(%agent_id, "recording hostname failed: {e}");
                        }
                    }
                }
                Message::AgentStatus(status) => {
                    let status = status.sanitized();
                    info!(%agent_id, users = ?status.users, local_ip = ?status.local_ip, os = ?status.os, "agent status");
                    if let Some(registry) = &hooks.registry {
                        if let Err(e) =
                            registry::record_status(&registry.pool, agent_id, &status).await
                        {
                            warn!(%agent_id, "recording status failed: {e}");
                        }
                    }
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

/// A viewer's connection and control stream.
struct ViewerConn<'a> {
    conn: &'a Connection,
    /// Protocol version from its `ViewerHello`.
    version: u32,
    /// Its Noise static key for this session.
    key: [u8; KEY_LEN],
    send: transport::SendStream,
    recv: transport::RecvStream,
}

/// How a viewer session went, for its `viewer.disconnect` record.
struct Ended {
    frames_sent: u64,
    /// The path it was on at the end.
    path: Path,
}

/// A viewer session with `grant.agent_id`: consent first, then video out
/// through the relay and sealed records both ways, until the session ends.
async fn serve_viewer(
    viewer: ViewerConn<'_>,
    pool: &PgPool,
    grant: &ViewerGrant,
    hooks: &Hooks,
    ended: &mut Ended,
) -> Result<(), ConnError> {
    let ViewerConn {
        conn,
        version,
        key,
        mut send,
        mut recv,
    } = viewer;
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
    if link.version < protocol::MIN_E2E_VERSION {
        // It would send the session in the clear (and, before protocol 4,
        // ignore consent): refuse. It updates itself.
        warn!(agent_id = %grant.agent_id, version = link.version, "agent too old for end-to-end encryption");
        viewers::record_consent(pool, grant, mode, Outcome::ConsentUnavailable).await?;
        return Err(ConnError::Outdated(
            "the agent is too old for end-to-end encrypted sessions; it will update itself",
        ));
    }
    // From here on, however this ends, the agent hears that it did.
    let _ended = EndSession {
        link: &link,
        session_id,
    };
    if mode == ConsentMode::Require {
        write_frame(&mut send, &Message::ConsentPending { timeout_secs }).await?;
    }
    // Which viewer the agent may complete the session's handshake with.
    link.send(Message::SessionViewerKey {
        session_id,
        public: key,
    });
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
    metrics::get()
        .sessions
        .get_or_create(&SessionLabels {
            mode: consent_mode_name(mode),
            outcome: outcome.as_str(),
        })
        .inc();
    if !outcome.allows_session() {
        info!(session_id, %outcome, "session refused");
        return Err(ConnError::ConsentRefused(outcome));
    }
    info!(session_id, %outcome, "session started");

    // --- Session ---------------------------------------------------------
    let viewer_id = hooks.hub.next_viewer_id();
    let mut monitors = link.monitors();
    let mut stream_monitor = link.stream_monitor();
    let mut sealed = link.route_sealed(session_id);

    let (to_viewer, mut outbox) = mpsc::unbounded_channel::<Message>();
    let _ = to_viewer.send(Message::ViewerWelcome {
        agent_id: grant.agent_id.clone(),
        monitors: monitors.borrow_and_update().clone(),
        active_monitor: *stream_monitor.borrow(),
    });
    // Viewers that can acknowledge frames take part in adaptive bitrate.
    let tracked = (version >= protocol::MIN_ADAPTIVE_VERSION).then(|| {
        let _ = to_viewer.send(Message::EnableFrameAcks);
        link.track_viewer(viewer_id)
    });
    let _ = to_viewer.send(hooks.peer.message());
    let mut subscription = link.subscribe(viewer_id);
    let mut video = conn.open_uni().await?;
    let _connected = ConnectedGuard::viewer(conn.kind());
    let mut path = PathGauge::relayed();
    let frames_sent = &mut ended.frames_sent;

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
            let metrics = metrics::get();
            metrics.relay_frames_out.inc();
            metrics.relay_bytes_out.inc_by(frame.payload.len() as u64);
            if let Some(tracked) = &tracked {
                tracked.forwarded(frame.seq);
            }
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
                data = sealed.records.recv() => match data {
                    Some(data) => { let _ = to_viewer.send(Message::Sealed { session_id, data }); }
                    None => break,
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
                Message::FrameAck { seq } if tracked.is_some() => {
                    if let Some(tracked) = &tracked {
                        tracked.acked(seq);
                    }
                }
                Message::Sealed { data, .. } => {
                    if data.len() > MAX_SEALED_LEN {
                        return Err(ConnError::Protocol("oversized sealed record"));
                    }
                    metrics::get().sealed_records.inc();
                    link.to_agent_sealed(session_id, data);
                }
                Message::PathReport(reported) => {
                    let before = path.current();
                    path.report(reported);
                    info!(session_id, from = %before, path = %reported, "session path");
                    if reported != Path::DirectFailed {
                        link.set_direct(viewer_id, reported == Path::Direct, tracked.as_ref());
                    }
                    viewers::record_path(pool, grant, reported).await?;
                }
                other => {
                    warn!(?other, "unexpected message from viewer");
                    return Err(ConnError::Protocol("unexpected message"));
                }
            }
        }
        Ok(())
    };

    let result = tokio::select! {
        r = writer => r,
        r = forward => r,
        r = notify => r,
        r = reader => r,
    };
    ended.path = path.current();
    result
}

fn enrollment_result(result: &'static str) {
    metrics::get()
        .enrollments
        .get_or_create(&ResultLabels { result })
        .inc();
}

fn consent_mode_name(mode: ConsentMode) -> &'static str {
    match mode {
        ConsentMode::Require => "require",
        ConsentMode::Notify => "notify",
        ConsentMode::Unattended => "unattended",
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

/// SHA-256 of the first certificate in `pem`, as `openssl x509 -fingerprint
/// -sha256` prints it (`AB:CD:…`).
pub fn pem_fingerprint(pem: &str) -> Result<String, common::tls::TlsError> {
    let certs = common::tls::certs_from_pem(pem)?;
    let hex = certs
        .first()
        .map(cert_fingerprint)
        .unwrap_or_default()
        .to_uppercase();
    let pairs: Vec<&str> = (0..hex.len()).step_by(2).map(|i| &hex[i..i + 2]).collect();
    Ok(pairs.join(":"))
}
