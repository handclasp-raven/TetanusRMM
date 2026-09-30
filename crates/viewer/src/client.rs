//! The viewer's connection: to the server, and when possible straight to
//! the agent.
//!
//! Sessions are end-to-end encrypted (see the `peer` crate). Before
//! [`connect`] returns, the viewer has completed a Noise handshake with the
//! agent through the server and checked the agent's certificate; from then
//! on input and clipboard are sealed records, and video arrives sealed and
//! is opened here. The server relays all of it unread.
//!
//! The session starts on the relay. If the server allows direct paths, the
//! viewer then gathers candidates, offers them to the agent (sealed), and
//! connects directly ([`peer::direct`]). Video arriving on both paths is
//! merged in sequence ([`peer::merge`]) so the switch is seamless; the
//! viewer then tells the agent and the server it is on the direct path
//! (`ViewerEvent::Path`). If the direct path drops, the relay takes over
//! again, with no new handshake.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peer::direct::{self, Credentials, DirectLink};
use peer::media::{MediaOpener, OpenError};
use peer::merge::{Merger, Source};
use peer::noise::{Inbox, Initiator, Opener, Sealer, StaticKey};
use peer::DirectSettings;
use protocol::clipboard::{ClipboardData, MAX_CLIPBOARD_BYTES};
use protocol::e2e::{Control, DirectAnswer, DirectOffer, Envelope, Path};
use protocol::input::InputEvent;
use protocol::media::{MediaFrame, MonitorInfo};
use protocol::{read_frame, write_frame, FrameError, Message, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use transport::{Connection, ConnectionError, Preference, TransportSettings};

pub struct ViewerOptions {
    /// Server QUIC (UDP) address.
    pub server: SocketAddr,
    /// Which transports to use, and where the WebSocket fallback is.
    pub transport: TransportSettings,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    /// PEM CA the server certificate must chain to. Agent certificates
    /// chain to it too: it is how the viewer knows the agent is genuine.
    pub ca_pem: String,
    /// Viewer-session token from `POST /api/agents/{id}/viewer-sessions`.
    pub token: String,
    pub bind: Option<SocketAddr>,
    /// Direct paths to the agent (the server can also turn them off).
    pub direct: DirectSettings,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Tls(#[from] common::TlsError),
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The server closed the connection, with its reason.
    #[error("server refused: {0}")]
    Refused(String),
    #[error("unexpected reply from server: {0:?}")]
    Unexpected(Box<Message>),
    /// The end-to-end handshake failed: most importantly, the agent could
    /// not prove it is the agent that was asked for.
    #[error("end-to-end encryption: {0}")]
    E2e(#[from] peer::E2eError),
    #[error("the agent did not complete the end-to-end handshake in time")]
    HandshakeTimedOut,
}

/// What the server sent after accepting the token.
#[derive(Debug, Clone)]
pub struct Welcome {
    pub agent_id: String,
    pub monitors: Vec<MonitorInfo>,
    pub active_monitor: Option<u32>,
}

#[derive(Debug)]
pub enum ViewerEvent {
    Monitors(Vec<MonitorInfo>),
    /// The stream now shows this monitor.
    StreamMonitor(u32),
    /// A video frame, decrypted: its payload is the `VideoPayload`.
    Frame(MediaFrame),
    /// The remote user's clipboard changed.
    Clipboard(ClipboardData),
    /// Video now flows this way (`Relayed` or `Direct`), or a direct
    /// attempt failed (`DirectFailed`: the session stays on the relay).
    Path(Path),
    /// The connection ended; the reason is human-readable.
    Closed(String),
}

/// How long the agent has to answer the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// After a direct path drops, wait this long before trying again...
pub const DIRECT_RETRY_AFTER: Duration = Duration::from_secs(10);
/// ...at most this many times per session.
pub const DIRECT_MAX_DROPS: u32 = 3;

/// Handle for the session: requests to the server, sealed records to the
/// agent.
#[derive(Clone)]
pub struct ViewerHandle {
    shared: Arc<Shared>,
}

struct Shared {
    /// Messages for the server's control stream.
    commands: mpsc::UnboundedSender<Message>,
    connection: Connection,
    sealer: Sealer,
    fingerprint: [u8; 32],
    direct: Mutex<DirectState>,
    media: Mutex<MediaState>,
    /// Newest frame delivered to the app, plus one (0 = none yet).
    received: AtomicU64,
    /// Whether the server wants frame acknowledgements.
    acks_enabled: AtomicBool,
    /// A media key arrived: frames held for it can be opened.
    key_added: tokio::sync::Notify,
}

#[derive(Default)]
struct DirectState {
    /// The direct path's control stream, while it is up.
    control: Option<mpsc::UnboundedSender<Envelope>>,
    connection: Option<quinn::Connection>,
    /// The agent's address on the direct path.
    remote: Option<SocketAddr>,
    attempt: Option<u32>,
    /// Video comes over the direct path now.
    in_use: bool,
}

/// Frames held while their key is on its way, at most (a few seconds).
const HELD_FRAMES: usize = 120;

#[derive(Default)]
struct MediaState {
    opener: MediaOpener,
    /// Frames sealed under a key that has not arrived yet (it travels as a
    /// record, so it can be overtaken), in order. Opened when it arrives.
    held: std::collections::VecDeque<MediaFrame>,
    /// Some were dropped for want of a key: ask for a keyframe once it
    /// arrives.
    awaiting_key: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ViewerHandle {
    /// Ask for a different monitor (applies to everyone watching this agent).
    pub fn select_monitor(&self, monitor: u32) {
        let _ = self
            .shared
            .commands
            .send(Message::SelectMonitor { monitor });
    }

    /// Ask for a keyframe, e.g. after a decode error.
    pub fn request_keyframe(&self) {
        let _ = self.shared.commands.send(Message::RequestKeyframe);
    }

    /// Inject input on the remote machine.
    pub fn send_input(&self, event: InputEvent) {
        self.shared.send_sealed(&Control::Input(event));
    }

    /// Put `data` on the remote clipboard.
    pub fn send_clipboard(&self, data: ClipboardData) {
        self.shared.send_sealed(&Control::Clipboard(data));
    }

    pub fn close(&self) {
        if let Some(direct) = lock(&self.shared.direct).connection.take() {
            direct.close(0u32.into(), b"viewer closed");
        }
        self.shared
            .connection
            .close(protocol::close_code::NORMAL, b"viewer closed");
    }

    /// Which transport the connection to the server runs on.
    pub fn transport(&self) -> transport::TransportKind {
        self.shared.connection.kind()
    }

    /// Whether video comes over the relay or a direct path.
    pub fn path(&self) -> Path {
        if lock(&self.shared.direct).in_use {
            Path::Direct
        } else {
            Path::Relayed
        }
    }

    /// The end-to-end session's fingerprint (the Noise handshake hash): the
    /// agent logs the same one. It never changes during a session, whatever
    /// the path.
    pub fn session_fingerprint(&self) -> [u8; 32] {
        self.shared.fingerprint
    }

    /// The agent's address on the direct path, while there is one.
    pub fn direct_remote(&self) -> Option<SocketAddr> {
        lock(&self.shared.direct).remote
    }

    /// Close the direct path, if there is one, as if the network had
    /// dropped it (the session falls back to the relay).
    pub fn drop_direct_path(&self) {
        if let Some(direct) = lock(&self.shared.direct).connection.as_ref() {
            direct.close(0u32.into(), b"dropped");
        }
    }
}

impl Shared {
    /// Seal `record` for the agent; over the direct path if it is up.
    fn send_sealed(&self, record: &Control) {
        let envelope = self.sealer.seal(record);
        let envelope = match &lock(&self.direct).control {
            Some(direct) => match direct.send(envelope) {
                Ok(()) => return,
                Err(mpsc::error::SendError(envelope)) => envelope,
            },
            None => envelope,
        };
        self.send_relay(&envelope);
    }

    fn send_relay(&self, envelope: &Envelope) {
        let _ = self.commands.send(Message::Sealed {
            session_id: 0,
            data: postcard::to_stdvec(envelope).expect("envelopes serialise"),
        });
    }

    fn report(&self, path: Path) {
        let _ = self.commands.send(Message::PathReport(path));
    }
}

/// Frames buffered between network and decoder.
const EVENT_QUEUE: usize = 64;

/// How often to acknowledge frames, once the server asks (adaptive
/// bitrate; the server's `relay::delay` allows for this interval). On a
/// direct path the acknowledgements go to the agent instead.
pub const ACK_INTERVAL: Duration = protocol::media::ACK_INTERVAL;

/// The server's reason for closing. The stream can end a moment before the
/// connection close (which carries the reason) arrives, so wait briefly.
async fn close_reason(conn: &Connection, fallback: impl std::fmt::Display) -> String {
    let _ = tokio::time::timeout(Duration::from_secs(1), conn.closed()).await;
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed { reason, .. }) => reason,
        Some(other) => other.to_string(),
        None => fallback.to_string(),
    }
}

/// Progress reported before the session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// The remote user is being asked to accept, for up to `timeout_secs`.
    WaitingForConsent { timeout_secs: u32 },
}

/// Connect, authenticate with the token, establish the end-to-end session
/// and start receiving.
pub async fn connect(
    options: &ViewerOptions,
) -> Result<(Welcome, ViewerHandle, mpsc::Receiver<ViewerEvent>), ClientError> {
    connect_with_status(options, |_| {}).await
}

fn sealed(envelope: &Envelope) -> Message {
    Message::Sealed {
        session_id: 0,
        data: postcard::to_stdvec(envelope).expect("envelopes serialise"),
    }
}

/// [`connect`], reporting progress (such as waiting for the remote user's
/// consent) to `status`. A refused session is [`ClientError::Refused`] with
/// the server's reason.
pub async fn connect_with_status(
    options: &ViewerOptions,
    mut status: impl FnMut(Pending),
) -> Result<(Welcome, ViewerHandle, mpsc::Receiver<ViewerEvent>), ClientError> {
    let ca = common::tls::certs_from_pem(&options.ca_pem)?;
    let target = options
        .transport
        .target(options.server, &options.server_name, options.bind);
    // No client certificate: the token is the credential. The token is
    // only spent once connected, so falling back cannot waste it.
    let dialer = transport::Dialer::new(target, &ca, None)?;
    let dialed = dialer
        .dial(&mut Preference::default())
        .await
        .map_err(|e| match e {
            transport::dial::DialError::Tls(e) => ClientError::Tls(e),
            other => ClientError::Unreachable(other.to_string()),
        })?;
    let (conn, endpoint) = (dialed.connection, dialed.endpoint);

    // This session's key; the server binds it to the token's session and
    // tells the agent, which accepts no other.
    let key = StaticKey::generate();
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::ViewerHello {
            token: options.token.clone(),
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    write_frame(
        &mut send,
        &Message::ViewerKey {
            public: key.public(),
        },
    )
    .await?;
    let welcome = loop {
        match read_frame::<_, Message>(&mut recv).await {
            Ok(Some(Message::ConsentPending { timeout_secs })) => {
                status(Pending::WaitingForConsent { timeout_secs });
            }
            Ok(Some(Message::ViewerWelcome {
                agent_id,
                monitors,
                active_monitor,
            })) => {
                break Welcome {
                    agent_id,
                    monitors,
                    active_monitor,
                }
            }
            Ok(Some(other)) => return Err(ClientError::Unexpected(Box::new(other))),
            Ok(None) => {
                return Err(ClientError::Refused(
                    close_reason(&conn, "connection closed").await,
                ))
            }
            Err(e) => return Err(ClientError::Refused(close_reason(&conn, e).await)),
        }
    };

    // --- End-to-end handshake, through the server ---------------------------
    let (initiator, first) = Initiator::start(&key, &welcome.agent_id)?;
    write_frame(&mut send, &sealed(&Envelope::Handshake(first))).await?;
    // The server sends session messages meanwhile; keep them for later.
    let mut early = Vec::new();
    let reply = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            match read_frame::<_, Message>(&mut recv).await? {
                Some(Message::Sealed { data, .. }) => match postcard::from_bytes(&data) {
                    Ok(Envelope::Handshake(reply)) => return Ok(reply),
                    _ => debug!("sealed message before the handshake finished dropped"),
                },
                Some(other) => early.push(other),
                None => {
                    return Err(ClientError::Refused(
                        close_reason(&conn, "connection closed").await,
                    ))
                }
            }
        }
    })
    .await
    .map_err(|_| ClientError::HandshakeTimedOut)??;
    let (last, session) = initiator.finish(&reply, &ca)?;
    write_frame(&mut send, &sealed(&Envelope::Handshake(last))).await?;
    info!(
        agent = %welcome.agent_id,
        fingerprint = %hex(&session.fingerprint[..8]),
        "end-to-end session established"
    );

    let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
    let (commands, outbox) = mpsc::unbounded_channel::<Message>();
    let shared = Arc::new(Shared {
        commands,
        connection: conn.clone(),
        sealer: session.sealer,
        fingerprint: session.fingerprint,
        direct: Mutex::new(DirectState::default()),
        media: Mutex::new(MediaState::default()),
        received: AtomicU64::new(0),
        acks_enabled: AtomicBool::new(false),
        key_added: tokio::sync::Notify::new(),
    });
    let handle = ViewerHandle {
        shared: shared.clone(),
    };
    tokio::spawn(run(
        Running {
            shared,
            conn,
            endpoint,
            send,
            recv,
            outbox,
            opener: session.opener,
            events: events_tx,
            direct: options.direct.clone(),
            server: options.server,
        },
        early,
    ));
    Ok((welcome, handle, events))
}

/// Everything the session's task owns.
struct Running {
    shared: Arc<Shared>,
    conn: Connection,
    endpoint: Option<quinn::Endpoint>,
    send: transport::SendStream,
    recv: transport::RecvStream,
    outbox: mpsc::UnboundedReceiver<Message>,
    opener: Opener,
    events: mpsc::Sender<ViewerEvent>,
    direct: DirectSettings,
    server: SocketAddr,
}

/// Channels between the session's parts.
struct Wiring {
    frames: mpsc::Sender<(Source, MediaFrame)>,
    envelopes: mpsc::UnboundedSender<Envelope>,
    /// Direct-path answers and refusals, for the attempt in progress.
    signals: mpsc::UnboundedSender<Control>,
    events: mpsc::Sender<ViewerEvent>,
}

impl Clone for Wiring {
    fn clone(&self) -> Self {
        Self {
            frames: self.frames.clone(),
            envelopes: self.envelopes.clone(),
            signals: self.signals.clone(),
            events: self.events.clone(),
        }
    }
}

async fn run(running: Running, early: Vec<Message>) {
    let Running {
        shared,
        conn,
        endpoint,
        mut send,
        mut recv,
        mut outbox,
        opener,
        events,
        direct: direct_settings,
        server,
    } = running;
    let (frames_tx, mut frames_rx) = mpsc::channel::<(Source, MediaFrame)>(EVENT_QUEUE);
    let (envelopes_tx, mut envelopes_rx) = mpsc::unbounded_channel::<Envelope>();
    let (signals_tx, signals_rx) = mpsc::unbounded_channel::<Control>();
    let wiring = Wiring {
        frames: frames_tx,
        envelopes: envelopes_tx,
        signals: signals_tx,
        events: events.clone(),
    };
    let mut signals_rx = Some(signals_rx);
    let mut direct_task: Option<tokio::task::JoinHandle<()>> = None;

    let writer = async {
        while let Some(msg) = outbox.recv().await {
            write_frame(&mut send, &msg).await?;
        }
        Ok::<(), ClientError>(())
    };

    let control = async {
        let mut early = early.into_iter();
        loop {
            let msg = match early.next() {
                Some(msg) => msg,
                None => match read_frame::<_, Message>(&mut recv).await? {
                    Some(msg) => msg,
                    None => return Ok(()),
                },
            };
            let event = match msg {
                Message::EnableFrameAcks => {
                    shared.acks_enabled.store(true, Ordering::Relaxed);
                    continue;
                }
                Message::PeerConfig { direct, stun_port } => {
                    if direct && direct_settings.enabled && direct_task.is_none() {
                        if let Some(signals) = signals_rx.take() {
                            direct_task = Some(tokio::spawn(direct_paths(
                                shared.clone(),
                                direct_settings.clone(),
                                server,
                                stun_port,
                                signals,
                                wiring.clone(),
                            )));
                        }
                    }
                    continue;
                }
                Message::Sealed { data, .. } => {
                    match postcard::from_bytes::<Envelope>(&data) {
                        Ok(envelope) => {
                            let _ = wiring.envelopes.send(envelope);
                        }
                        Err(e) => warn!("malformed sealed message: {e}"),
                    }
                    continue;
                }
                Message::MonitorList { monitors } => ViewerEvent::Monitors(monitors),
                Message::StreamMonitor { monitor } => ViewerEvent::StreamMonitor(monitor),
                other => {
                    debug!(?other, "ignoring control message");
                    continue;
                }
            };
            if events.send(event).await.is_err() {
                return Ok(());
            }
        }
    };

    // Video through the relay.
    let relay_video = async {
        let mut stream = conn.accept_uni().await?;
        while let Some(frame) = read_frame::<_, MediaFrame>(&mut stream).await? {
            if wiring.frames.send((Source::Relay, frame)).await.is_err() {
                break;
            }
        }
        Ok(())
    };

    // Video from both paths, in order and opened, to the app.
    let video = async {
        let mut merger = Merger::default();
        loop {
            let delivered: Vec<MediaFrame> = tokio::select! {
                item = frames_rx.recv() => match item {
                    Some((source, frame)) => arrived(&shared, &mut merger, source, frame)
                        .await
                        .into_iter()
                        .map(|(_, frame)| frame)
                        .collect(),
                    None => return,
                },
                () = sleep_until(merger.deadline()) => merger
                    .expire(std::time::Instant::now())
                    .into_iter()
                    .map(|(_, frame)| frame)
                    .collect(),
                // Frames held for this key can go now (with any delivered
                // meanwhile, which queued up behind them).
                () = shared.key_added.notified() => Vec::new(),
            };
            let opened = open_frames(&shared, delivered);
            for plain in opened {
                let seq = plain.seq;
                // Blocks while the app is behind: then acknowledgements
                // stall too, and the sender sees this viewer lagging.
                if events.send(ViewerEvent::Frame(plain)).await.is_err() {
                    return;
                }
                shared.received.store(seq + 1, Ordering::Relaxed);
            }
        }
    };

    // Sealed records from the agent, from both paths, opened and in order.
    let records = async {
        let mut inbox = Inbox::default();
        loop {
            let ready = match inbox.deadline() {
                Some(deadline) => tokio::select! {
                    envelope = envelopes_rx.recv() => match envelope {
                        Some(envelope) => open(&opener, &mut inbox, envelope),
                        None => return,
                    },
                    () = tokio::time::sleep_until(deadline.into()) => {
                        inbox.expire(std::time::Instant::now())
                    }
                },
                None => match envelopes_rx.recv().await {
                    Some(envelope) => open(&opener, &mut inbox, envelope),
                    None => return,
                },
            };
            for record in ready {
                match record {
                    Control::MediaKey(key) => {
                        debug!(epoch = key.epoch, "media key");
                        let mut media = lock(&shared.media);
                        media.opener.add(&key);
                        if std::mem::take(&mut media.awaiting_key) {
                            let _ = shared.commands.send(Message::RequestKeyframe);
                        }
                        shared.key_added.notify_one();
                    }
                    Control::Clipboard(data) if data.size() <= MAX_CLIPBOARD_BYTES => {
                        if events.send(ViewerEvent::Clipboard(data)).await.is_err() {
                            return;
                        }
                    }
                    record @ (Control::DirectAnswer(_) | Control::DirectFailed { .. }) => {
                        let _ = wiring.signals.send(record);
                    }
                    other => debug!(?other, "record ignored"),
                }
            }
        }
    };

    let acks = async {
        let mut ticker = tokio::time::interval(ACK_INTERVAL);
        let mut acked = 0;
        loop {
            ticker.tick().await;
            let newest = shared.received.load(Ordering::Relaxed);
            if newest == acked {
                continue;
            }
            acked = newest;
            if lock(&shared.direct).in_use {
                shared.send_sealed(&Control::FrameAck { seq: newest - 1 });
            } else if shared.acks_enabled.load(Ordering::Relaxed) {
                let _ = shared.commands.send(Message::FrameAck { seq: newest - 1 });
            }
        }
    };

    let result = tokio::select! {
        r = writer => r,
        r = control => r,
        r = relay_video => r,
        () = video => Ok(()),
        () = records => Ok(()),
        () = acks => unreachable!("acknowledging never ends"),
    };
    if let Some(task) = direct_task {
        task.abort();
    }
    if let Some(direct) = lock(&shared.direct).connection.take() {
        direct.close(0u32.into(), b"session over");
    }
    let reason = match result {
        Ok(()) => close_reason(&conn, "stream ended").await,
        Err(e) => close_reason(&conn, e).await,
    };
    let _ = events.send(ViewerEvent::Closed(reason)).await;
    conn.close(protocol::close_code::NORMAL, b"");
    if let Some(endpoint) = endpoint {
        endpoint.close(protocol::close_code::NORMAL.into(), b"");
    }
}

/// A frame arrived from `source`. The first one from a direct path means
/// that path now carries every frame: switch to it.
async fn arrived(
    shared: &Shared,
    merger: &mut Merger,
    source: Source,
    frame: MediaFrame,
) -> Vec<(Source, MediaFrame)> {
    if source == Source::Direct {
        let switched = {
            let mut direct = lock(&shared.direct);
            match direct.attempt {
                Some(attempt) if !direct.in_use => {
                    direct.in_use = true;
                    Some(attempt)
                }
                _ => None,
            }
        };
        if let Some(attempt) = switched {
            info!(attempt, "video now on the direct path");
            // To the agent (over the direct path): it may stop relaying.
            shared.send_sealed(&Control::DirectInUse { attempt });
            // To the server: stop forwarding; counted as direct.
            shared.report(Path::Direct);
        }
    }
    merger.push(frame, source, std::time::Instant::now())
}

/// Sleep until `deadline`, or forever if there is none.
async fn sleep_until(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

/// Open frames, in order: first any held for their key, then `delivered`.
/// Stops at the first frame whose key has not arrived; it and everything
/// after it wait (keys only move forward, so later frames need it too).
fn open_frames(shared: &Shared, delivered: Vec<MediaFrame>) -> Vec<MediaFrame> {
    let mut media = lock(&shared.media);
    media.held.extend(delivered);
    let mut out = Vec::new();
    while let Some(frame) = media.held.pop_front() {
        match media.opener.open(&frame) {
            Ok(plain) => out.push(plain),
            Err(OpenError::UnknownEpoch(epoch)) => {
                debug!(seq = frame.seq, epoch, "frame held until its key arrives");
                media.held.push_front(frame);
                break;
            }
            Err(OpenError::Unauthentic) => {
                warn!(seq = frame.seq, "frame failed authentication; dropped");
            }
        }
    }
    if media.held.len() > HELD_FRAMES {
        let excess = media.held.len() - HELD_FRAMES;
        media.held.drain(..excess);
        media.awaiting_key = true;
        warn!(
            dropped = excess,
            "no key for held frames; dropping the oldest"
        );
    }
    out
}

fn open(opener: &Opener, inbox: &mut Inbox, envelope: Envelope) -> Vec<Control> {
    let Envelope::Record { nonce, ciphertext } = envelope else {
        debug!("handshake message after the handshake ignored");
        return Vec::new();
    };
    if !inbox.wants(nonce) {
        return Vec::new();
    }
    match opener.open(nonce, &ciphertext) {
        Ok(record) => inbox.accept(nonce, record, std::time::Instant::now()),
        Err(e) => {
            warn!("sealed record rejected: {e}");
            Vec::new()
        }
    }
}

// --- Direct paths ---------------------------------------------------------

/// Try for a direct path, and keep one while the session lasts: after one
/// drops, try again a little later (a few times). A failed attempt is not
/// retried: the network between the peers will not change its mind.
async fn direct_paths(
    shared: Arc<Shared>,
    settings: DirectSettings,
    server: SocketAddr,
    stun_port: Option<u16>,
    mut signals: mpsc::UnboundedReceiver<Control>,
    wiring: Wiring,
) {
    tokio::time::sleep(settings.delay).await;
    let mut drops = 0;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match try_direct(&shared, &settings, server, stun_port, &mut signals, attempt).await {
            Ok(link) => {
                info!(attempt, remote = %link.remote(), "direct path up");
                let _ = wiring.events.send(ViewerEvent::Path(Path::Direct)).await;
                carry_direct(&shared, link, attempt, &wiring).await;
                drops += 1;
                if drops >= DIRECT_MAX_DROPS {
                    return;
                }
                tokio::time::sleep(DIRECT_RETRY_AFTER).await;
            }
            Err(e) => {
                info!(attempt, "no direct path, staying on the relay: {e}");
                lock(&shared.direct).attempt = None;
                shared.send_sealed(&Control::DirectFailed { attempt });
                shared.report(Path::DirectFailed);
                let _ = wiring
                    .events
                    .send(ViewerEvent::Path(Path::DirectFailed))
                    .await;
                return;
            }
        }
    }
}

async fn try_direct(
    shared: &Shared,
    settings: &DirectSettings,
    server: SocketAddr,
    stun_port: Option<u16>,
    signals: &mut mpsc::UnboundedReceiver<Control>,
    attempt: u32,
) -> Result<DirectLink, String> {
    let gathered = settings
        .gather(server, stun_port)
        .await
        .map_err(|e| format!("gathering candidates: {e}"))?;
    let credentials = Credentials::generate();
    debug!(attempt, candidates = ?gathered.candidates, reflexive = ?gathered.reflexive, "offering a direct path");
    lock(&shared.direct).attempt = Some(attempt);
    shared.send_sealed(&Control::DirectOffer(DirectOffer {
        attempt,
        candidates: gathered.candidates.clone(),
        cert_sha256: credentials.sha256,
    }));
    let answer: DirectAnswer = tokio::time::timeout(settings.timeout, async {
        while let Some(signal) = signals.recv().await {
            match signal {
                Control::DirectAnswer(answer) if answer.attempt == attempt => return Ok(answer),
                Control::DirectFailed { attempt: a } if a == attempt => {
                    return Err("the agent could not open a direct path".to_owned())
                }
                _ => {}
            }
        }
        Err("session ended".to_owned())
    })
    .await
    .map_err(|_| "the agent did not answer".to_owned())??;
    debug!(attempt, candidates = ?answer.candidates, "agent answered");
    direct::connect(
        gathered.socket,
        &credentials,
        answer.cert_sha256,
        &answer.candidates,
        attempt,
        settings.timeout,
    )
    .await
    .map_err(|e| e.to_string())
}

/// Use a direct path until it drops, then fall back to the relay.
async fn carry_direct(shared: &Shared, link: DirectLink, attempt: u32, wiring: &Wiring) {
    let DirectLink {
        connection,
        endpoint,
        mut send,
        mut recv,
    } = link;
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Envelope>();
    {
        let mut direct = lock(&shared.direct);
        direct.control = Some(control_tx);
        direct.remote = Some(connection.remote_address());
        direct.connection = Some(connection.clone());
    }
    let writer = async {
        while let Some(envelope) = control_rx.recv().await {
            write_frame(&mut send, &envelope)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok::<(), String>(())
    };
    let reader = async {
        while let Some(envelope) = read_frame::<_, Envelope>(&mut recv)
            .await
            .map_err(|e| e.to_string())?
        {
            let _ = wiring.envelopes.send(envelope);
        }
        Ok(())
    };
    let video = async {
        let mut stream = connection.accept_uni().await.map_err(|e| e.to_string())?;
        while let Some(frame) = read_frame::<_, MediaFrame>(&mut stream)
            .await
            .map_err(|e| e.to_string())?
        {
            if wiring.frames.send((Source::Direct, frame)).await.is_err() {
                break;
            }
        }
        Ok(())
    };
    let result = tokio::select! {
        r = writer => r,
        r = reader => r,
        r = video => r,
    };
    let reason = connection
        .close_reason()
        .map(|r| r.to_string())
        .or(result.err())
        .unwrap_or_else(|| "closed".into());
    connection.close(0u32.into(), b"");
    endpoint.close(0u32.into(), b"");
    let was_in_use = {
        let mut direct = lock(&shared.direct);
        let was = direct.in_use;
        *direct = DirectState::default();
        was
    };
    warn!(attempt, reason, "direct path dropped; back on the relay");
    if was_in_use {
        // The relay restarts this viewer on a keyframe.
        shared.report(Path::Relayed);
        let _ = wiring.events.send(ViewerEvent::Path(Path::Relayed)).await;
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
