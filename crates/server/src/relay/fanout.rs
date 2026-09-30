//! Fan-out of one agent's video stream to many viewers. Pure logic, no I/O:
//! the async plumbing in `super` feeds it events and carries out the
//! [`AgentAction`]s it returns.
//!
//! The agent encodes once; every subscribed viewer gets the same frames.
//!
//! - A viewer can only start decoding at a keyframe, so a new subscriber
//!   is held back until the next one, and the agent is asked for one.
//! - The first subscriber starts the agent's stream; the last one leaving
//!   stops it, so an unwatched agent captures nothing.
//! - Each viewer has a bounded queue. If a viewer falls behind and its
//!   queue fills, it drops to "waiting for keyframe" and a keyframe is
//!   requested, rather than blocking the agent or the other viewers.
//! - Selecting a monitor applies to everyone watching the agent, because
//!   there is one stream.
//! - A viewer whose session moved to a direct path gets its video from the
//!   agent, so it is skipped (but still counts as watching). If it falls
//!   back to the relay, it waits for a keyframe, which is requested at once.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::media::MediaFrame;
use tokio::sync::mpsc::{self, error::TrySendError};

pub type ViewerId = u64;

/// Frames queued per viewer before it is considered too slow.
pub const VIEWER_QUEUE: usize = 64;

/// Minimum spacing of keyframe requests to the agent.
pub const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

/// What the relay must tell the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    Start { monitor: u32 },
    Stop,
    RequestKeyframe,
}

#[derive(Debug)]
struct Subscriber {
    tx: mpsc::Sender<Arc<MediaFrame>>,
    waiting_for_keyframe: bool,
    /// On a direct path: the relay sends it nothing.
    direct: bool,
}

#[derive(Debug, Default)]
pub struct Fanout {
    subscribers: HashMap<ViewerId, Subscriber>,
    /// Monitor being streamed; `None` when the agent's stream is stopped.
    monitor: Option<u32>,
    last_keyframe_request: Option<Instant>,
}

impl Fanout {
    pub fn monitor(&self) -> Option<u32> {
        self.monitor
    }

    pub fn viewers(&self) -> usize {
        self.subscribers.len()
    }

    /// Add a viewer. `default_monitor` is used if nothing is streaming yet.
    pub fn subscribe(
        &mut self,
        id: ViewerId,
        tx: mpsc::Sender<Arc<MediaFrame>>,
        default_monitor: u32,
        now: Instant,
    ) -> Vec<AgentAction> {
        self.subscribers.insert(
            id,
            Subscriber {
                tx,
                waiting_for_keyframe: true,
                direct: false,
            },
        );
        match self.monitor {
            None => {
                self.monitor = Some(default_monitor);
                self.last_keyframe_request = Some(now);
                vec![AgentAction::Start {
                    monitor: default_monitor,
                }]
            }
            // Joining a running stream: ask for a keyframe (the new viewer
            // needs one), bypassing the throttle so it never waits long.
            Some(_) => {
                self.last_keyframe_request = Some(now);
                vec![AgentAction::RequestKeyframe]
            }
        }
    }

    pub fn unsubscribe(&mut self, id: ViewerId) -> Vec<AgentAction> {
        if self.subscribers.remove(&id).is_some()
            && self.subscribers.is_empty()
            && self.monitor.take().is_some()
        {
            return vec![AgentAction::Stop];
        }
        vec![]
    }

    /// Switch the (shared) stream to `monitor`.
    pub fn select_monitor(&mut self, monitor: u32, now: Instant) -> Vec<AgentAction> {
        if self.subscribers.is_empty() || self.monitor == Some(monitor) {
            return vec![];
        }
        self.monitor = Some(monitor);
        // Frames from the old monitor must not be mixed with the new one.
        for sub in self.subscribers.values_mut() {
            sub.waiting_for_keyframe = true;
        }
        self.last_keyframe_request = Some(now);
        vec![AgentAction::Start { monitor }]
    }

    /// Viewer `id`'s session moved to a direct path (`true`) or back to the
    /// relay (`false`).
    pub fn set_direct(&mut self, id: ViewerId, direct: bool, now: Instant) -> Vec<AgentAction> {
        let Some(sub) = self.subscribers.get_mut(&id) else {
            return vec![];
        };
        if sub.direct == direct {
            return vec![];
        }
        sub.direct = direct;
        if direct || self.monitor.is_none() {
            return vec![];
        }
        // Frames went past while it was direct: restart it on a keyframe,
        // without waiting out the throttle.
        sub.waiting_for_keyframe = true;
        self.last_keyframe_request = Some(now);
        vec![AgentAction::RequestKeyframe]
    }

    /// A viewer's decoder lost sync and wants a fresh keyframe.
    pub fn viewer_requests_keyframe(&mut self, id: ViewerId, now: Instant) -> Vec<AgentAction> {
        if let Some(sub) = self.subscribers.get_mut(&id) {
            sub.waiting_for_keyframe = true;
        }
        self.throttled_keyframe_request(now)
    }

    /// Deliver one frame from the agent to every subscriber.
    pub fn on_frame(&mut self, frame: Arc<MediaFrame>, now: Instant) -> Vec<AgentAction> {
        let mut need_keyframe = false;
        let mut closed = Vec::new();
        for (id, sub) in &mut self.subscribers {
            if sub.direct || sub.waiting_for_keyframe && !frame.keyframe {
                continue;
            }
            match sub.tx.try_send(frame.clone()) {
                Ok(()) => sub.waiting_for_keyframe = false,
                Err(TrySendError::Full(_)) => {
                    sub.waiting_for_keyframe = true;
                    need_keyframe = true;
                    crate::metrics::get().relay_frames_dropped.inc();
                }
                Err(TrySendError::Closed(_)) => closed.push(*id),
            }
        }
        let mut actions = Vec::new();
        for id in closed {
            actions.extend(self.unsubscribe(id));
        }
        if need_keyframe && self.monitor.is_some() {
            actions.extend(self.throttled_keyframe_request(now));
        }
        actions
    }

    /// The agent's stream went away (agent disconnected or stream ended).
    pub fn reset(&mut self) {
        self.monitor = None;
        for sub in self.subscribers.values_mut() {
            sub.waiting_for_keyframe = true;
        }
    }

    fn throttled_keyframe_request(&mut self, now: Instant) -> Vec<AgentAction> {
        if self
            .last_keyframe_request
            .is_some_and(|t| now.duration_since(t) < KEYFRAME_REQUEST_INTERVAL)
        {
            return vec![];
        }
        self.last_keyframe_request = Some(now);
        vec![AgentAction::RequestKeyframe]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn frame(seq: u64, keyframe: bool) -> Arc<MediaFrame> {
        Arc::new(MediaFrame {
            seq,
            keyframe,
            payload: vec![seq as u8],
        })
    }

    fn drain(rx: &mut mpsc::Receiver<Arc<MediaFrame>>) -> Vec<u64> {
        let mut seqs = Vec::new();
        while let Ok(f) = rx.try_recv() {
            seqs.push(f.seq);
        }
        seqs
    }

    #[test]
    fn first_viewer_starts_and_last_viewer_stops_the_stream() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, _ra) = mpsc::channel(8);
        let (b, _rb) = mpsc::channel(8);
        assert_eq!(f.subscribe(1, a, 2, t), [AgentAction::Start { monitor: 2 }]);
        assert_eq!(f.monitor(), Some(2));
        assert_eq!(f.subscribe(2, b, 0, t), [AgentAction::RequestKeyframe]);
        assert_eq!(f.unsubscribe(1), []);
        assert_eq!(f.unsubscribe(2), [AgentAction::Stop]);
        assert_eq!(f.monitor(), None);
        assert_eq!(f.unsubscribe(2), [], "unknown viewer is a no-op");
    }

    #[test]
    fn every_viewer_gets_every_frame_from_one_encode() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, mut ra) = mpsc::channel(8);
        let (b, mut rb) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        f.subscribe(2, b, 0, t);
        for (seq, key) in [(0, true), (1, false), (2, false)] {
            assert_eq!(f.on_frame(frame(seq, key), t), []);
        }
        assert_eq!(drain(&mut ra), [0, 1, 2]);
        assert_eq!(drain(&mut rb), [0, 1, 2]);
    }

    #[test]
    fn a_late_joiner_starts_at_the_next_keyframe() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, mut ra) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        f.on_frame(frame(0, true), t);
        f.on_frame(frame(1, false), t);

        let (b, mut rb) = mpsc::channel(8);
        assert_eq!(f.subscribe(2, b, 0, t), [AgentAction::RequestKeyframe]);
        f.on_frame(frame(2, false), t);
        f.on_frame(frame(3, true), t);
        f.on_frame(frame(4, false), t);
        assert_eq!(drain(&mut ra), [0, 1, 2, 3, 4]);
        assert_eq!(drain(&mut rb), [3, 4]);
    }

    #[test]
    fn a_slow_viewer_resyncs_without_holding_back_others() {
        let mut f = Fanout::default();
        let t0 = Instant::now();
        let (fast, mut rfast) = mpsc::channel(16);
        let (slow, mut rslow) = mpsc::channel(2);
        f.subscribe(1, fast, 0, t0);
        f.subscribe(2, slow, 0, t0);

        let t = t0 + KEYFRAME_REQUEST_INTERVAL;
        f.on_frame(frame(0, true), t);
        f.on_frame(frame(1, false), t);
        // The slow viewer's queue is full now: frame 2 overflows it.
        assert_eq!(
            f.on_frame(frame(2, false), t),
            [AgentAction::RequestKeyframe]
        );
        // Further overflows within the interval do not spam the agent.
        assert_eq!(f.on_frame(frame(3, false), t + MS), []);
        assert_eq!(drain(&mut rslow), [0, 1]);
        // The slow viewer skips deltas until the next keyframe...
        f.on_frame(frame(4, false), t + MS * 2);
        f.on_frame(frame(5, true), t + MS * 3);
        f.on_frame(frame(6, false), t + MS * 4);
        assert_eq!(drain(&mut rslow), [5, 6]);
        // ...while the fast viewer saw everything.
        assert_eq!(drain(&mut rfast), [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn switching_monitor_restarts_everyone_on_a_keyframe() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, mut ra) = mpsc::channel(8);
        let (b, mut rb) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        f.subscribe(2, b, 0, t);
        f.on_frame(frame(0, true), t);
        assert_eq!(f.select_monitor(1, t), [AgentAction::Start { monitor: 1 }]);
        assert_eq!(f.select_monitor(1, t), [], "already on monitor 1");
        // A straggling delta from the old monitor is not delivered.
        f.on_frame(frame(1, false), t);
        f.on_frame(frame(2, true), t);
        assert_eq!(drain(&mut ra), [0, 2]);
        assert_eq!(drain(&mut rb), [0, 2]);
    }

    #[test]
    fn disconnected_viewers_are_dropped_and_can_stop_the_stream() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, ra) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        drop(ra);
        assert_eq!(f.on_frame(frame(0, true), t), [AgentAction::Stop]);
        assert_eq!(f.viewers(), 0);
    }

    #[test]
    fn viewer_keyframe_requests_are_throttled() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, _ra) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        assert_eq!(f.viewer_requests_keyframe(1, t + MS), []);
        let later = t + KEYFRAME_REQUEST_INTERVAL;
        assert_eq!(
            f.viewer_requests_keyframe(1, later),
            [AgentAction::RequestKeyframe]
        );
        assert_eq!(f.viewer_requests_keyframe(1, later + MS), []);
    }

    #[test]
    fn reset_holds_viewers_until_a_new_keyframe() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, mut ra) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        f.on_frame(frame(0, true), t);
        f.reset();
        assert_eq!(f.monitor(), None);
        f.on_frame(frame(1, false), t);
        f.on_frame(frame(2, true), t);
        assert_eq!(drain(&mut ra), [0, 2]);
    }

    #[test]
    fn direct_viewers_are_skipped_and_resume_on_a_keyframe() {
        let mut f = Fanout::default();
        let t = Instant::now();
        let (a, mut ra) = mpsc::channel(8);
        let (b, mut rb) = mpsc::channel(8);
        f.subscribe(1, a, 0, t);
        f.subscribe(2, b, 0, t);
        f.on_frame(frame(0, true), t);
        assert_eq!((drain(&mut ra), drain(&mut rb)), (vec![0], vec![0]));

        assert_eq!(f.set_direct(2, true, t), []);
        f.on_frame(frame(1, false), t);
        assert_eq!((drain(&mut ra), drain(&mut rb)), (vec![1], vec![]));
        assert_eq!(f.viewers(), 2, "a direct viewer still counts");

        // Back on the relay: a keyframe is requested straight away, and
        // nothing reaches it until one comes.
        assert_eq!(f.set_direct(2, false, t), [AgentAction::RequestKeyframe]);
        assert_eq!(f.set_direct(2, false, t), [], "no change, no request");
        f.on_frame(frame(2, false), t);
        f.on_frame(frame(3, true), t);
        assert_eq!(drain(&mut rb), [3]);
        assert_eq!(drain(&mut ra), [2, 3]);

        // The last viewer leaving still stops the stream, direct or not.
        f.set_direct(1, true, t);
        f.unsubscribe(2);
        assert_eq!(f.unsubscribe(1), [AgentAction::Stop]);
    }
}
