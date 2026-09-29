//! The media relay. Agents sit behind NAT and only ever connect out to the
//! server, so viewers connect to the server too, and the server relays:
//!
//! ```text
//!  agent ──(one encoded stream)──▶ server ──▶ viewer A
//!                                    │  └───▶ viewer B
//!                                    └ fan-out: one encode, N viewers
//! ```
//!
//! [`Hub`] tracks every connected agent's [`AgentLink`]. A link holds the
//! channel to the agent's control-stream writer, the agent's monitor list,
//! and the [`fanout::Fanout`] that decides who gets which frame and when the
//! agent must start/stop streaming or send a keyframe.
//!
//! Frames are relayed as opaque [`MediaFrame`]s: the server reads only the
//! `seq`/`keyframe` header, never the payload (see `protocol::media`).
//!
//! The link also carries interactive sessions: consent requests and the
//! agent's decisions, the agent's clipboard (to every viewer), and the
//! user's Ctrl+F12 kill switch (every viewer is dropped).
//!
//! And it closes the adaptive-bitrate loop (see `agent::media::rate`): it
//! notes when each frame arrived, times viewers' acknowledgements against
//! that ([`delay::ViewerDelay`]), and a few times a second tells the agent
//! how its video is being delivered (`StreamReport`).

pub mod delay;
pub mod fanout;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::clipboard::ClipboardData;
use protocol::consent::{Outcome, SessionRequest};
use protocol::media::{MediaFrame, MonitorInfo, SendLog, StreamReport};
use protocol::{Message, MIN_ADAPTIVE_VERSION};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use delay::ViewerDelay;
use fanout::{AgentAction, Fanout, ViewerId, VIEWER_QUEUE};

/// How often the agent hears how its video is being delivered (while
/// frames arrive).
pub const REPORT_INTERVAL: Duration = Duration::from_millis(200);

/// What the relay knows about the agent's video delivery.
#[derive(Default)]
struct Delivery {
    /// When each recent frame arrived from the agent.
    arrivals: SendLog,
    /// Payload bytes received on this connection.
    bytes: u64,
    last_report: Option<Instant>,
    /// Each acknowledging viewer's delay tracker.
    viewers: HashMap<ViewerId, Arc<Mutex<ViewerDelay>>>,
}

/// One connected agent, as seen by the relay.
pub struct AgentLink {
    pub agent_id: String,
    /// Protocol version from the agent's Hello.
    pub version: u32,
    /// Messages to write on the agent's control stream.
    to_agent: mpsc::UnboundedSender<Message>,
    fanout: Mutex<Fanout>,
    monitors: watch::Sender<Vec<MonitorInfo>>,
    /// Monitor currently streamed (`None` = stopped). Viewers watch this.
    stream_monitor: watch::Sender<Option<u32>>,
    /// Flips to true when the agent disconnects.
    closed: watch::Sender<bool>,
    /// Session requests awaiting the agent's consent decision.
    decisions: Mutex<HashMap<u64, oneshot::Sender<Outcome>>>,
    /// The remote user's clipboard, to every viewer.
    clipboard: broadcast::Sender<ClipboardData>,
    /// Bumped each time the user presses Ctrl+F12.
    terminations: watch::Sender<u64>,
    /// The agent's connection, for opening remote-operation streams
    /// (`None` in unit tests).
    connection: Option<transport::Connection>,
    /// Interactive shells open right now (see [`AgentLink::shell_started`]).
    shells: AtomicUsize,
    delivery: Mutex<Delivery>,
}

/// Counts one open shell on its agent until dropped.
pub struct ActiveShell(Arc<AgentLink>);

impl Drop for ActiveShell {
    fn drop(&mut self) {
        self.0.shells.fetch_sub(1, Ordering::Relaxed);
        crate::metrics::get().shells_open.dec();
    }
}

/// Clipboard updates buffered per viewer.
const CLIPBOARD_QUEUE: usize = 4;

/// A viewer's subscription. Unsubscribes on drop.
pub struct Subscription {
    pub id: ViewerId,
    pub frames: mpsc::Receiver<Arc<MediaFrame>>,
    link: Arc<AgentLink>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let actions = self.link.fanout().unsubscribe(self.id);
        self.link.apply(actions);
    }
}

impl AgentLink {
    fn fanout(&self) -> std::sync::MutexGuard<'_, Fanout> {
        self.fanout.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn apply(&self, actions: Vec<AgentAction>) {
        for action in actions {
            let msg = match action {
                AgentAction::Start { monitor } => {
                    self.stream_monitor.send_replace(Some(monitor));
                    Message::StartStream { monitor }
                }
                AgentAction::Stop => {
                    self.stream_monitor.send_replace(None);
                    Message::StopStream
                }
                AgentAction::RequestKeyframe => Message::RequestKeyframe,
            };
            let _ = self.to_agent.send(msg);
        }
    }

    pub fn monitors(&self) -> watch::Receiver<Vec<MonitorInfo>> {
        self.monitors.subscribe()
    }

    pub fn stream_monitor(&self) -> watch::Receiver<Option<u32>> {
        self.stream_monitor.subscribe()
    }

    pub fn closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }

    pub fn viewers(&self) -> usize {
        self.fanout().viewers()
    }

    /// Interactive shells open on this agent.
    pub fn shells(&self) -> usize {
        self.shells.load(Ordering::Relaxed)
    }

    /// Count a shell as open until the returned guard is dropped.
    pub fn shell_started(self: &Arc<Self>) -> ActiveShell {
        self.shells.fetch_add(1, Ordering::Relaxed);
        crate::metrics::get().shells_open.inc();
        ActiveShell(self.clone())
    }

    /// Send a message to the agent (input, clipboard).
    pub fn send(&self, message: Message) {
        let _ = self.to_agent.send(message);
    }

    fn decisions(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Outcome>>> {
        self.decisions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the agent to apply its consent policy to a new session. The
    /// receiver yields the agent's decision.
    pub fn request_session(&self, request: SessionRequest) -> oneshot::Receiver<Outcome> {
        let (tx, rx) = oneshot::channel();
        self.decisions().insert(request.session_id, tx);
        self.send(Message::SessionRequest(request));
        rx
    }

    /// The session is over (or abandoned while consent was pending): tell
    /// the agent, which releases held keys and updates the tray.
    pub fn end_session(&self, session_id: u64) {
        self.decisions().remove(&session_id);
        self.send(Message::SessionEnded { session_id });
    }

    /// The user's clipboard changes, for one viewer.
    pub fn clipboard(&self) -> broadcast::Receiver<ClipboardData> {
        self.clipboard.subscribe()
    }

    /// Changes when the user presses Ctrl+F12.
    pub fn terminations(&self) -> watch::Receiver<u64> {
        self.terminations.subscribe()
    }

    /// Open a new bidirectional stream to the agent (shell, script, file
    /// transfer). `None` if this link has no connection (unit tests).
    pub async fn open_stream(
        &self,
    ) -> Option<Result<(transport::SendStream, transport::RecvStream), transport::ConnectionError>>
    {
        Some(self.connection.as_ref()?.open_bi().await)
    }

    /// Which transport the agent is connected over.
    pub fn transport(&self) -> Option<transport::TransportKind> {
        self.connection.as_ref().map(transport::Connection::kind)
    }

    /// The monitor a new viewer sees if nothing is streaming yet: the
    /// primary, else the first.
    fn default_monitor(&self) -> u32 {
        let monitors = self.monitors.borrow();
        monitors
            .iter()
            .find(|m| m.primary)
            .or(monitors.first())
            .map_or(0, |m| m.id)
    }

    /// Start receiving this agent's frames.
    pub fn subscribe(self: &Arc<Self>, id: ViewerId) -> Subscription {
        let (tx, rx) = mpsc::channel(VIEWER_QUEUE);
        let default = self.default_monitor();
        let actions = self.fanout().subscribe(id, tx, default, Instant::now());
        self.apply(actions);
        Subscription {
            id,
            frames: rx,
            link: self.clone(),
        }
    }

    pub fn select_monitor(&self, monitor: u32) {
        let actions = self.fanout().select_monitor(monitor, Instant::now());
        self.apply(actions);
    }

    pub fn viewer_requests_keyframe(&self, id: ViewerId) {
        let actions = self.fanout().viewer_requests_keyframe(id, Instant::now());
        self.apply(actions);
    }

    // --- called by the agent's connection task ---------------------------

    pub fn set_monitors(&self, monitors: Vec<MonitorInfo>) {
        self.monitors.send_replace(monitors);
    }

    /// The agent decided consent for a session.
    pub fn on_decision(&self, session_id: u64, outcome: Outcome) {
        if let Some(tx) = self.decisions().remove(&session_id) {
            let _ = tx.send(outcome);
        }
    }

    /// The agent's user copied something.
    pub fn on_clipboard(&self, data: ClipboardData) {
        let _ = self.clipboard.send(data);
    }

    /// The agent's user pressed Ctrl+F12: every viewer must go.
    pub fn user_terminated(&self) {
        self.terminations.send_modify(|n| *n += 1);
    }

    pub fn on_frame(&self, frame: MediaFrame) {
        let now = Instant::now();
        let report = self.record_arrival(&frame, now);
        let actions = self.fanout().on_frame(Arc::new(frame), now);
        self.apply(actions);
        if let Some(report) = report {
            self.send(Message::StreamReport(report));
        }
    }

    fn delivery(&self) -> std::sync::MutexGuard<'_, Delivery> {
        self.delivery.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Note a frame's arrival; returns a report for the agent if one is due.
    fn record_arrival(&self, frame: &MediaFrame, now: Instant) -> Option<StreamReport> {
        let metrics = crate::metrics::get();
        metrics.relay_frames_in.inc();
        metrics.relay_bytes_in.inc_by(frame.payload.len() as u64);
        let mut delivery = self.delivery();
        delivery.arrivals.record(frame.seq, now);
        delivery.bytes += frame.payload.len() as u64;
        if self.version < MIN_ADAPTIVE_VERSION
            || delivery
                .last_report
                .is_some_and(|t| now.duration_since(t) < REPORT_INTERVAL)
        {
            return None;
        }
        delivery.last_report = Some(now);
        let viewer_delay = delivery
            .viewers
            .values()
            .map(|v| v.lock().unwrap_or_else(|e| e.into_inner()).current(now))
            .max()
            .unwrap_or_default();
        if !delivery.viewers.is_empty() {
            metrics.viewer_delay.observe(viewer_delay.as_secs_f64());
        }
        Some(StreamReport {
            seq: frame.seq,
            bytes: delivery.bytes,
            viewer_delay_ms: u32::try_from(viewer_delay.as_millis()).unwrap_or(u32::MAX),
        })
    }

    /// When frame `seq` arrived from the agent, if it was recent.
    pub fn arrived_at(&self, seq: u64) -> Option<Instant> {
        self.delivery().arrivals.sent_at(seq)
    }

    /// Track viewer `id`'s delay (it acknowledges frames) until the guard
    /// is dropped.
    pub fn track_viewer(self: &Arc<Self>, id: ViewerId) -> TrackedViewer {
        let tracker = Arc::new(Mutex::new(ViewerDelay::default()));
        self.delivery().viewers.insert(id, tracker.clone());
        TrackedViewer {
            link: self.clone(),
            id,
            tracker,
        }
    }

    /// The agent's video stream ended (e.g. it restarted capture); viewers
    /// wait for the next keyframe.
    pub fn stream_reset(&self) {
        let mut fanout = self.fanout();
        let monitor = fanout.monitor();
        fanout.reset();
        drop(fanout);
        // If viewers are still watching, ask the agent to resume.
        if let Some(monitor) = monitor {
            let mut fanout = self.fanout();
            if fanout.viewers() > 0 {
                let actions = fanout.select_monitor(monitor, Instant::now());
                drop(fanout);
                self.apply(actions);
            }
        }
    }
}

/// A viewer whose delay counts towards the agent's bitrate; stops counting
/// when dropped.
pub struct TrackedViewer {
    link: Arc<AgentLink>,
    id: ViewerId,
    tracker: Arc<Mutex<ViewerDelay>>,
}

impl TrackedViewer {
    fn tracker(&self) -> std::sync::MutexGuard<'_, ViewerDelay> {
        self.tracker.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Frame `seq` was written to the viewer.
    pub fn forwarded(&self, seq: u64) {
        if let Some(arrived) = self.link.arrived_at(seq) {
            self.tracker().forwarded(seq, arrived);
        }
    }

    /// The viewer acknowledged everything up to `seq`.
    pub fn acked(&self, seq: u64) {
        self.tracker().acked(seq, Instant::now());
    }
}

impl Drop for TrackedViewer {
    fn drop(&mut self) {
        self.link.delivery().viewers.remove(&self.id);
    }
}

/// All connected agents.
#[derive(Default)]
pub struct Hub {
    agents: Mutex<HashMap<String, Arc<AgentLink>>>,
    next_viewer: AtomicU64,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn agents(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AgentLink>>> {
        self.agents.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register a newly connected agent. `to_agent` feeds its control-stream
    /// writer; `connection` is used to open streams to it. Replaces (and
    /// closes) any previous link for the same id.
    pub fn register(
        &self,
        agent_id: &str,
        version: u32,
        to_agent: mpsc::UnboundedSender<Message>,
        connection: Option<transport::Connection>,
    ) -> Arc<AgentLink> {
        let link = Arc::new(AgentLink {
            agent_id: agent_id.to_owned(),
            version,
            to_agent,
            fanout: Mutex::new(Fanout::default()),
            monitors: watch::channel(Vec::new()).0,
            stream_monitor: watch::channel(None).0,
            closed: watch::channel(false).0,
            decisions: Mutex::new(HashMap::new()),
            clipboard: broadcast::channel(CLIPBOARD_QUEUE).0,
            terminations: watch::channel(0).0,
            connection,
            shells: AtomicUsize::new(0),
            delivery: Mutex::new(Delivery::default()),
        });
        if let Some(old) = self.agents().insert(agent_id.to_owned(), link.clone()) {
            old.closed.send_replace(true);
        }
        link
    }

    /// Remove `link` if it is still the current one for its agent.
    pub fn unregister(&self, link: &Arc<AgentLink>) {
        link.closed.send_replace(true);
        let mut agents = self.agents();
        if agents
            .get(&link.agent_id)
            .is_some_and(|current| Arc::ptr_eq(current, link))
        {
            agents.remove(&link.agent_id);
        }
    }

    pub fn get(&self, agent_id: &str) -> Option<Arc<AgentLink>> {
        self.agents().get(agent_id).cloned()
    }

    pub fn is_online(&self, agent_id: &str) -> bool {
        self.agents().contains_key(agent_id)
    }

    pub fn next_viewer_id(&self) -> ViewerId {
        self.next_viewer.fetch_add(1, Ordering::Relaxed) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: u32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            id,
            name: format!("DISPLAY{id}"),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Message>) -> Vec<Message> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    #[tokio::test]
    async fn subscription_drives_the_agent_and_cleans_up_on_drop() {
        let hub = Hub::new();
        let (tx, mut to_agent) = mpsc::unbounded_channel();
        let link = hub.register("agt-1", 4, tx, None);
        link.set_monitors(vec![monitor(0, false), monitor(1, true)]);

        let mut a = link.subscribe(hub.next_viewer_id());
        // Default is the primary monitor.
        assert_eq!(drain(&mut to_agent), [Message::StartStream { monitor: 1 }]);
        assert_eq!(*link.stream_monitor().borrow(), Some(1));

        let b = link.subscribe(hub.next_viewer_id());
        assert_eq!(drain(&mut to_agent), [Message::RequestKeyframe]);

        link.on_frame(MediaFrame {
            seq: 0,
            keyframe: true,
            payload: vec![1],
        });
        assert_eq!(a.frames.try_recv().unwrap().seq, 0);

        drop(b);
        assert_eq!(drain(&mut to_agent), []);
        drop(a);
        assert_eq!(drain(&mut to_agent), [Message::StopStream]);
        assert_eq!(*link.stream_monitor().borrow(), None);
    }

    #[tokio::test]
    async fn reconnecting_agent_replaces_and_closes_the_old_link() {
        let hub = Hub::new();
        let old = hub.register("agt-1", 4, mpsc::unbounded_channel().0, None);
        let new = hub.register("agt-1", 4, mpsc::unbounded_channel().0, None);
        assert!(*old.closed().borrow());
        assert!(Arc::ptr_eq(&hub.get("agt-1").unwrap(), &new));
        // The old connection ending must not unregister the new one.
        hub.unregister(&old);
        assert!(hub.is_online("agt-1"));
        hub.unregister(&new);
        assert!(!hub.is_online("agt-1"));
    }

    #[tokio::test]
    async fn consent_decisions_reach_the_waiting_viewer_and_ending_is_forwarded() {
        let hub = Hub::new();
        let (tx, mut to_agent) = mpsc::unbounded_channel();
        let link = hub.register("agt-1", 4, tx, None);
        let request = SessionRequest {
            session_id: 7,
            technician: "jane".into(),
            mode: protocol::consent::ConsentMode::Require,
            on_no_user: protocol::consent::OnNoUser::Deny,
            timeout_secs: 30,
        };
        let decision = link.request_session(request.clone());
        assert_eq!(drain(&mut to_agent), [Message::SessionRequest(request)]);
        link.on_decision(99, Outcome::Granted); // unknown: ignored
        link.on_decision(7, Outcome::Denied);
        assert_eq!(decision.await.unwrap(), Outcome::Denied);

        link.end_session(7);
        assert_eq!(
            drain(&mut to_agent),
            [Message::SessionEnded { session_id: 7 }]
        );
    }

    #[tokio::test]
    async fn clipboard_and_kill_switch_reach_every_viewer() {
        let hub = Hub::new();
        let link = hub.register("agt-1", 4, mpsc::unbounded_channel().0, None);
        let (mut a, mut b) = (link.clipboard(), link.clipboard());
        let (mut ka, mut kb) = (link.terminations(), link.terminations());
        link.on_clipboard(ClipboardData::Text("hi".into()));
        for rx in [&mut a, &mut b] {
            assert_eq!(rx.recv().await.unwrap(), ClipboardData::Text("hi".into()));
        }
        link.user_terminated();
        for rx in [&mut ka, &mut kb] {
            rx.changed().await.unwrap();
        }
    }

    #[tokio::test]
    async fn open_shells_are_counted_until_their_guards_drop() {
        let hub = Hub::new();
        let link = hub.register("agt-1", 6, mpsc::unbounded_channel().0, None);
        assert_eq!(link.shells(), 0);
        let a = link.shell_started();
        let b = link.shell_started();
        assert_eq!(link.shells(), 2);
        drop(a);
        assert_eq!(link.shells(), 1);
        drop(b);
        assert_eq!(link.shells(), 0);
    }

    #[tokio::test]
    async fn stream_reset_resumes_for_remaining_viewers() {
        let hub = Hub::new();
        let (tx, mut to_agent) = mpsc::unbounded_channel();
        let link = hub.register("agt-1", 4, tx, None);
        let _a = link.subscribe(1);
        drain(&mut to_agent);
        link.stream_reset();
        assert_eq!(drain(&mut to_agent), [Message::StartStream { monitor: 0 }]);
    }
}
