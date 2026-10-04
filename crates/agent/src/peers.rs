//! End-to-end encrypted sessions and their direct paths, on the agent.
//!
//! For each session the server announces (`SessionViewerKey`), [`Peers`]
//! completes the Noise handshake with that viewer only, then:
//!
//! - opens the viewer's sealed records (from the relay or a direct path),
//!   restores their order, and hands input and clipboard to the connection
//!   loop as [`Inbound`] (which still applies consent and the kill switch);
//! - seals what the agent sends it (clipboard, the media key, signaling),
//!   over the direct path when there is one, else through the relay;
//! - seals video once for everyone ([`Peers::seal_frame`]), rotating the
//!   media key whenever a viewer leaves;
//! - answers the viewer's `DirectOffer`s: gathers candidates, punches,
//!   listens, and once the viewer connects, feeds that viewer video
//!   directly. When every viewer takes video directly, nothing goes through
//!   the relay ([`Peers::relay_needed`]); when a direct path drops, the
//!   relay takes over again.
//!
//! The server never sees any of it: it sees `Sealed` bytes and sealed
//! frames, and hears from the viewer which path is in use.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peer::direct::{Credentials, DirectError, DirectLink, Listening};
use peer::media::MediaSealer;
use peer::noise::{Inbox, Opener, Responder, Sealer, StaticKey};
use peer::DirectSettings;
use peer::E2eError;
use protocol::e2e::{AgentProof, Control, DirectAnswer, DirectOffer, Envelope, KEY_LEN};
use protocol::media::{MediaFrame, ViewerDelay};
use protocol::{read_frame, write_frame, Message};
use tokio::sync::mpsc::{self, error::TrySendError, UnboundedSender};
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

/// Frames queued per direct viewer before it counts as falling behind.
pub const DIRECT_QUEUE: usize = 64;

/// Minimum spacing of keyframes the agent asks its own encoder for on
/// behalf of direct viewers.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(500);

/// What the connection loop must act on.
#[derive(Debug)]
pub enum Inbound {
    /// A record from `session_id`'s viewer, in the order it was sealed.
    Record { session_id: u64, record: Control },
    /// A direct viewer fell behind or lost its path: encode a keyframe.
    Keyframe,
}

/// What does not change for the life of a connection.
pub struct PeerSetup {
    pub agent_id: String,
    pub key: Arc<StaticKey>,
    /// `None` if the certificate key could not sign one: no session can be
    /// established (logged when the connection starts).
    pub proof: Option<AgentProof>,
    pub settings: DirectSettings,
    /// The server's QUIC address: its host serves STUN, and its family is
    /// the one direct sockets use.
    pub server: SocketAddr,
}

pub struct Peers {
    setup: PeerSetup,
    /// Messages for the server's control stream.
    outbox: UnboundedSender<Message>,
    inbound: UnboundedSender<Inbound>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// From the server's `PeerConfig`: may sessions go direct, and where
    /// its STUN responder is. Until it says, they may not.
    direct_allowed: bool,
    stun_port: Option<u16>,
    sessions: HashMap<u64, PeerSession>,
    media: MediaSealer,
    last_keyframe_request: Option<Instant>,
}

struct PeerSession {
    viewer_key: [u8; KEY_LEN],
    handshake: Handshake,
    direct: Option<Direct>,
    /// A direct attempt in progress.
    attempt: Option<(u32, AbortHandle)>,
}

enum Handshake {
    Waiting,
    Replied(Box<Responder>),
    Done(Box<Keys>),
    Failed,
}

struct Keys {
    sealer: Arc<Sealer>,
    opener: Opener,
    inbox: Inbox,
}

/// A viewer's direct path.
struct Direct {
    attempt: u32,
    control: UnboundedSender<Envelope>,
    video: mpsc::Sender<Arc<MediaFrame>>,
    waiting_for_keyframe: bool,
    /// The viewer takes its video from here (it said `DirectInUse`), so
    /// the relay need not carry it.
    in_use: bool,
    delay: ViewerDelay,
    task: AbortHandle,
}

impl Drop for Direct {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Peers {
    pub fn new(
        setup: PeerSetup,
        outbox: UnboundedSender<Message>,
        inbound: UnboundedSender<Inbound>,
    ) -> Arc<Self> {
        Arc::new(Self {
            setup,
            outbox,
            inbound,
            inner: Mutex::new(Inner {
                direct_allowed: false,
                stun_port: None,
                sessions: HashMap::new(),
                media: MediaSealer::new(),
                last_keyframe_request: None,
            }),
        })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// The server's `PeerConfig`.
    pub fn set_policy(&self, direct: bool, stun_port: Option<u16>) {
        let mut inner = self.inner();
        inner.direct_allowed = direct;
        inner.stun_port = stun_port;
    }

    /// The server announced session `session_id`'s viewer key.
    pub fn expect(&self, session_id: u64, viewer_key: [u8; KEY_LEN]) {
        self.inner().sessions.insert(
            session_id,
            PeerSession {
                viewer_key,
                handshake: Handshake::Waiting,
                direct: None,
                attempt: None,
            },
        );
    }

    /// Session `session_id` is over. The media key changes, so its viewer
    /// cannot read what follows.
    pub fn end(&self, session_id: u64) {
        let mut inner = self.inner();
        let Some(removed) = inner.sessions.remove(&session_id) else {
            return;
        };
        if let Some((_, task)) = removed.attempt {
            task.abort();
        }
        if matches!(removed.handshake, Handshake::Done(_)) {
            self.rotate_media_key(&mut inner);
        }
    }

    /// Every session is over (connection lost, or the kill switch).
    pub fn end_all(&self) {
        let mut inner = self.inner();
        for (_, session) in inner.sessions.drain() {
            if let Some((_, task)) = session.attempt {
                task.abort();
            }
        }
        inner.media.rotate();
    }

    fn rotate_media_key(&self, inner: &mut Inner) {
        let key = inner.media.rotate().clone();
        debug!(epoch = key.epoch, "media key rotated");
        for (&session_id, session) in &inner.sessions {
            self.send_in(session_id, session, &Control::MediaKey(key.clone()));
        }
    }

    /// A `Sealed` message from the server for `session_id`.
    pub fn on_sealed(self: &Arc<Self>, session_id: u64, data: &[u8]) {
        match postcard::from_bytes::<Envelope>(data) {
            Ok(envelope) => self.on_envelope(session_id, envelope),
            Err(e) => warn!(session_id, "malformed sealed message: {e}"),
        }
    }

    /// An envelope from `session_id`'s viewer, from either path.
    fn on_envelope(self: &Arc<Self>, session_id: u64, envelope: Envelope) {
        let mut inner = self.inner();
        let Some(session) = inner.sessions.get_mut(&session_id) else {
            debug!(session_id, "sealed message for an unknown session dropped");
            return;
        };
        match envelope {
            Envelope::Handshake(message) => self.handshake(session_id, &mut inner, &message),
            Envelope::Record { nonce, ciphertext } => {
                let Handshake::Done(keys) = &mut session.handshake else {
                    debug!(session_id, "record before the handshake finished dropped");
                    return;
                };
                if !keys.inbox.wants(nonce) {
                    return;
                }
                let ready = match keys.opener.open(nonce, &ciphertext) {
                    Ok(record) => keys.inbox.accept(nonce, record, Instant::now()),
                    Err(e @ E2eError::UnknownRecord(_)) => {
                        debug!(session_id, "{e}; skipped");
                        keys.inbox.unreadable(nonce, Instant::now())
                    }
                    Err(e) => return warn!(session_id, "sealed record rejected: {e}"),
                };
                drop(inner);
                for record in ready {
                    self.dispatch(session_id, record);
                }
            }
        }
    }

    fn handshake(self: &Arc<Self>, session_id: u64, inner: &mut Inner, message: &[u8]) {
        let key = inner.media.key().clone();
        let session = inner
            .sessions
            .get_mut(&session_id)
            .expect("checked by caller");
        let step = std::mem::replace(&mut session.handshake, Handshake::Failed);
        session.handshake = match step {
            Handshake::Waiting => {
                let Some(proof) = &self.setup.proof else {
                    warn!(
                        session_id,
                        "cannot prove this agent's identity; session refused"
                    );
                    return;
                };
                let reply = Responder::new(
                    &self.setup.key,
                    &self.setup.agent_id,
                    session.viewer_key,
                    proof,
                )
                .and_then(|mut responder| {
                    let reply = responder.reply(message)?;
                    Ok((responder, reply))
                });
                match reply {
                    Ok((responder, reply)) => {
                        self.send_relay(session_id, &Envelope::Handshake(reply));
                        Handshake::Replied(Box::new(responder))
                    }
                    Err(e) => {
                        warn!(session_id, "handshake failed: {e}");
                        Handshake::Failed
                    }
                }
            }
            Handshake::Replied(responder) => match responder.finish(message) {
                Ok(session_keys) => {
                    info!(
                        session_id,
                        fingerprint = %hex(&session_keys.fingerprint[..8]),
                        "end-to-end session established"
                    );
                    let keys = Box::new(Keys {
                        sealer: Arc::new(session_keys.sealer),
                        opener: session_keys.opener,
                        inbox: Inbox::default(),
                    });
                    self.send_relay_bytes(
                        session_id,
                        keys.sealer.seal_bytes(&Control::MediaKey(key)),
                    );
                    Handshake::Done(keys)
                }
                Err(e) => {
                    warn!(session_id, "handshake failed: {e}");
                    Handshake::Failed
                }
            },
            done @ Handshake::Done(_) => {
                warn!(session_id, "unexpected handshake message ignored");
                done
            }
            Handshake::Failed => Handshake::Failed,
        };
    }

    /// Act on a record: peer-level ones here, the rest in the loop.
    fn dispatch(self: &Arc<Self>, session_id: u64, record: Control) {
        match record {
            Control::DirectOffer(offer) => self.start_attempt(session_id, offer),
            Control::DirectFailed { attempt } => {
                let mut inner = self.inner();
                if let Some(session) = inner.sessions.get_mut(&session_id) {
                    if session.attempt.as_ref().is_some_and(|(a, _)| *a == attempt) {
                        if let Some((_, task)) = session.attempt.take() {
                            task.abort();
                        }
                    }
                }
            }
            Control::DirectInUse { attempt } => {
                let mut inner = self.inner();
                if let Some(direct) = inner
                    .sessions
                    .get_mut(&session_id)
                    .and_then(|s| s.direct.as_mut())
                    .filter(|d| d.attempt == attempt)
                {
                    direct.in_use = true;
                    info!(session_id, "video now on the direct path");
                }
            }
            Control::FrameAck { seq } => {
                if let Some(direct) = self
                    .inner()
                    .sessions
                    .get_mut(&session_id)
                    .and_then(|s| s.direct.as_mut())
                {
                    direct.delay.acked(seq, Instant::now());
                }
            }
            // Only the viewer sends these; ignore them from it.
            Control::MediaKey(_)
            | Control::DirectAnswer(_)
            | Control::StreamStatus(_)
            | Control::CredentialStatus(_)
            | Control::TypeTextStatus(_) => {}
            record @ (Control::Input(_)
            | Control::Clipboard(_)
            | Control::StreamSettings(_)
            | Control::SecureAttention
            | Control::CredentialRequest
            | Control::CredentialType
            | Control::CredentialForget
            | Control::TypeText { .. }) => {
                let _ = self.inbound.send(Inbound::Record { session_id, record });
            }
        }
    }

    /// Give up on reordering gaps that have waited long enough.
    pub fn expire(self: &Arc<Self>) {
        let now = Instant::now();
        let mut ready = Vec::new();
        for (&session_id, session) in &mut self.inner().sessions {
            if let Handshake::Done(keys) = &mut session.handshake {
                ready.extend(keys.inbox.expire(now).into_iter().map(|r| (session_id, r)));
            }
        }
        for (session_id, record) in ready {
            self.dispatch(session_id, record);
        }
    }

    /// Seal `record` for `session_id`'s viewer and send it.
    pub fn send(&self, session_id: u64, record: &Control) {
        let inner = self.inner();
        if let Some(session) = inner.sessions.get(&session_id) {
            self.send_in(session_id, session, record);
        }
    }

    /// Seal `record` for each of `sessions` that is established.
    pub fn send_each(&self, sessions: &[u64], record: &Control) {
        let inner = self.inner();
        for id in sessions {
            if let Some(session) = inner.sessions.get(id) {
                self.send_in(*id, session, record);
            }
        }
    }

    fn send_in(&self, session_id: u64, session: &PeerSession, record: &Control) {
        let Handshake::Done(keys) = &session.handshake else {
            return;
        };
        let envelope = keys.sealer.seal(record);
        if let Some(direct) = &session.direct {
            match direct.control.send(envelope) {
                Ok(()) => return,
                // The path just died: the relay still works.
                Err(mpsc::error::SendError(envelope)) => {
                    return self.send_relay(session_id, &envelope)
                }
            }
        }
        self.send_relay(session_id, &envelope);
    }

    fn send_relay(&self, session_id: u64, envelope: &Envelope) {
        self.send_relay_bytes(
            session_id,
            postcard::to_stdvec(envelope).expect("envelopes serialise"),
        );
    }

    fn send_relay_bytes(&self, session_id: u64, data: Vec<u8>) {
        let _ = self.outbox.send(Message::Sealed { session_id, data });
    }

    // --- Video --------------------------------------------------------------

    /// Seal a frame (payload: the plaintext `VideoPayload`) for everyone.
    pub fn seal_frame(&self, frame: MediaFrame) -> Arc<MediaFrame> {
        Arc::new(self.inner().media.seal(frame))
    }

    /// Whether anyone still needs video through the relay: yes unless
    /// every session's viewer takes it over a direct path.
    pub fn relay_needed(&self) -> bool {
        let inner = self.inner();
        inner.sessions.is_empty()
            || inner
                .sessions
                .values()
                .any(|s| !s.direct.as_ref().is_some_and(|d| d.in_use))
    }

    /// Queue a sealed frame for each direct viewer. One that falls behind
    /// skips to the next keyframe, which is requested, rather than holding
    /// up the encoder or the other viewers.
    pub fn feed_direct(&self, frame: &Arc<MediaFrame>, now: Instant) {
        let mut inner = self.inner();
        let mut need_keyframe = false;
        for direct in inner
            .sessions
            .values_mut()
            .filter_map(|s| s.direct.as_mut())
        {
            if direct.waiting_for_keyframe && !frame.keyframe {
                continue;
            }
            match direct.video.try_send(frame.clone()) {
                Ok(()) => {
                    direct.waiting_for_keyframe = false;
                    direct.delay.forwarded(frame.seq, now);
                }
                Err(TrySendError::Full(_)) => {
                    direct.waiting_for_keyframe = true;
                    need_keyframe = true;
                }
                Err(TrySendError::Closed(_)) => {}
            }
        }
        if need_keyframe {
            self.request_keyframe(&mut inner, now);
        }
    }

    fn request_keyframe(&self, inner: &mut Inner, now: Instant) {
        if inner
            .last_keyframe_request
            .is_some_and(|t| now.duration_since(t) < KEYFRAME_INTERVAL)
        {
            return;
        }
        inner.last_keyframe_request = Some(now);
        let _ = self.inbound.send(Inbound::Keyframe);
    }

    /// The worst queueing delay among viewers taking video directly, or
    /// `None` if there are none (for adaptive bitrate).
    pub fn direct_delay(&self, now: Instant) -> Option<Duration> {
        self.inner()
            .sessions
            .values()
            .filter_map(|s| s.direct.as_ref().filter(|d| d.in_use))
            .map(|d| d.delay.current(now))
            .max()
    }

    // --- Direct paths -------------------------------------------------------

    fn start_attempt(self: &Arc<Self>, session_id: u64, offer: DirectOffer) {
        let attempt = offer.attempt;
        let mut inner = self.inner();
        let allowed = self.setup.settings.enabled && inner.direct_allowed;
        let stun_port = inner.stun_port;
        let Some(session) = inner.sessions.get_mut(&session_id) else {
            return;
        };
        if !allowed {
            debug!(session_id, "direct paths are off; staying on the relay");
            self.send_in(session_id, session, &Control::DirectFailed { attempt });
            return;
        }
        // A new offer replaces whatever came before.
        if let Some((_, task)) = session.attempt.take() {
            task.abort();
        }
        session.direct = None;
        let peers = self.clone();
        let task = tokio::spawn(async move {
            match peers.attempt(session_id, offer, stun_port).await {
                Ok(link) => peers.direct_up(session_id, attempt, link),
                Err(e) => {
                    info!(session_id, attempt, "no direct path: {e}");
                    peers.send(session_id, &Control::DirectFailed { attempt });
                    if let Some(session) = peers.inner().sessions.get_mut(&session_id) {
                        if session.attempt.as_ref().is_some_and(|(a, _)| *a == attempt) {
                            session.attempt = None;
                        }
                    }
                }
            }
        });
        session.attempt = Some((attempt, task.abort_handle()));
    }

    async fn attempt(
        &self,
        session_id: u64,
        offer: DirectOffer,
        stun_port: Option<u16>,
    ) -> Result<DirectLink, DirectError> {
        let gathered = self
            .setup
            .settings
            .gather(self.setup.server, stun_port)
            .await?;
        let credentials = Credentials::generate();
        info!(
            session_id,
            attempt = offer.attempt,
            ours = ?gathered.candidates,
            theirs = ?offer.candidates,
            "trying a direct path"
        );
        let listening = Listening::start(
            gathered.socket,
            &credentials,
            offer.cert_sha256,
            &offer.candidates,
            offer.attempt,
        )?;
        self.send(
            session_id,
            &Control::DirectAnswer(DirectAnswer {
                attempt: offer.attempt,
                candidates: gathered.candidates,
                cert_sha256: credentials.sha256,
            }),
        );
        listening.accept(self.setup.settings.timeout).await
    }

    fn direct_up(self: &Arc<Self>, session_id: u64, attempt: u32, link: DirectLink) {
        let mut inner = self.inner();
        let Some(session) = inner
            .sessions
            .get_mut(&session_id)
            .filter(|s| s.attempt.as_ref().is_some_and(|(a, _)| *a == attempt))
        else {
            link.close();
            return;
        };
        info!(session_id, attempt, remote = %link.remote(), "direct path up");
        session.attempt = None;
        let (control, control_rx) = mpsc::unbounded_channel();
        let (video, video_rx) = mpsc::channel(DIRECT_QUEUE);
        let task = tokio::spawn(
            self.clone()
                .run_direct(session_id, attempt, link, control_rx, video_rx),
        );
        session.direct = Some(Direct {
            attempt,
            control,
            video,
            waiting_for_keyframe: false,
            in_use: false,
            delay: ViewerDelay::default(),
            task: task.abort_handle(),
        });
    }

    /// Carry a direct path until it drops.
    async fn run_direct(
        self: Arc<Self>,
        session_id: u64,
        attempt: u32,
        link: DirectLink,
        mut control_rx: mpsc::UnboundedReceiver<Envelope>,
        mut video_rx: mpsc::Receiver<Arc<MediaFrame>>,
    ) {
        let DirectLink {
            connection,
            endpoint,
            mut send,
            mut recv,
        } = link;
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
                self.on_envelope(session_id, envelope);
            }
            Ok(())
        };
        let video = async {
            let mut stream = connection.open_uni().await.map_err(|e| e.to_string())?;
            while let Some(frame) = video_rx.recv().await {
                write_frame(&mut stream, frame.as_ref())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        };
        let result = tokio::select! {
            r = writer => r,
            r = reader => r,
            r = video => r,
        };
        let reason = match (result, connection.close_reason()) {
            (_, Some(reason)) => reason.to_string(),
            (Err(e), None) => e,
            (Ok(()), None) => "closed".into(),
        };
        connection.close(0u32.into(), b"");
        endpoint.close(0u32.into(), b"");
        self.direct_down(session_id, attempt, &reason);
    }

    fn direct_down(&self, session_id: u64, attempt: u32, reason: &str) {
        let mut inner = self.inner();
        let Some(session) = inner.sessions.get_mut(&session_id) else {
            return;
        };
        if session.direct.as_ref().is_none_or(|d| d.attempt != attempt) {
            return;
        }
        let was_in_use = session.direct.as_ref().is_some_and(|d| d.in_use);
        // This runs at the very end of the path's own task, so the abort
        // in `Direct::drop` has nothing left to cancel.
        session.direct = None;
        warn!(
            session_id,
            attempt, reason, "direct path dropped; back on the relay"
        );
        if was_in_use {
            let now = Instant::now();
            inner.last_keyframe_request = None;
            self.request_keyframe(&mut inner, now);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
