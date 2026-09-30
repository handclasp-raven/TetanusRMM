//! Screen streaming types.
//!
//! Video flows agent -> server -> viewers on dedicated QUIC unidirectional
//! streams (one per stream start), separate from the control stream, as a
//! sequence of framed [`MediaFrame`]s.
//!
//! The server is a relay: it routes and fans out frames but never looks
//! inside [`MediaFrame::payload`]. It only reads the small header (`seq`,
//! `keyframe`), which it needs to start each new viewer on a keyframe. That
//! split is deliberate: Phase 10 encrypts the payload end to end between
//! agent and viewer, and the relay keeps working unchanged.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// One display attached to the agent's desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    /// Stable within one monitor list; used to select a monitor.
    pub id: u32,
    /// OS name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    /// Position on the virtual desktop.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// A unit of video on a media stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFrame {
    /// Increments per frame within one stream.
    pub seq: u64,
    /// Decoding can start here (IDR with SPS/PPS).
    pub keyframe: bool,
    /// Encoded [`VideoPayload`]. Opaque to the server.
    pub payload: Vec<u8>,
}

/// What the viewer decodes. Only agent and viewer ever see this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoPayload {
    pub monitor: u32,
    /// Capture time, microseconds since the stream started.
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
    /// One H.264 access unit, Annex B, Constrained Baseline profile.
    pub h264: Vec<u8>,
}

impl VideoPayload {
    pub fn to_frame(&self, seq: u64, keyframe: bool) -> MediaFrame {
        MediaFrame {
            seq,
            keyframe,
            payload: postcard::to_stdvec(self).expect("payload serialises"),
        }
    }
}

impl MediaFrame {
    /// Decode the payload. Only the viewer does this.
    pub fn video(&self) -> Result<VideoPayload, postcard::Error> {
        postcard::from_bytes(&self.payload)
    }
}

/// Delivery feedback for the agent's video (see `Message::StreamReport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamReport {
    /// Newest frame the server has received from the agent. The agent
    /// times the round trip from when it sent that frame.
    pub seq: u64,
    /// Payload bytes received on the agent's video so far (this
    /// connection).
    pub bytes: u64,
    /// Worst queueing delay among the viewers that acknowledge frames, in
    /// milliseconds: how far the slowest viewer's link is behind.
    pub viewer_delay_ms: u32,
}

/// When each recent frame was sent (or relayed), by sequence number, to
/// time acknowledgements. Keeps the last [`SendLog::CAPACITY`] frames.
#[derive(Debug, Default)]
pub struct SendLog {
    sent: VecDeque<(u64, Instant)>,
}

impl SendLog {
    pub const CAPACITY: usize = 512;

    pub fn record(&mut self, seq: u64, at: Instant) {
        if self.sent.len() == Self::CAPACITY {
            self.sent.pop_front();
        }
        self.sent.push_back((seq, at));
    }

    /// When `seq` was sent, if it is still in the log.
    pub fn sent_at(&self, seq: u64) -> Option<Instant> {
        // Sequence numbers only grow, so this is sorted.
        let i = self.sent.binary_search_by_key(&seq, |(s, _)| *s).ok()?;
        Some(self.sent[i].1)
    }

    /// Forget everything (a new stream starts).
    pub fn clear(&mut self) {
        self.sent.clear();
    }
}

/// Queueing delay from round-trip samples: each sample minus the smallest
/// round trip seen within [`QueueDelay::WINDOW`], which stands in for the
/// path's delay with empty queues. A sliding minimum, so a path that
/// genuinely got slower is re-learned after the window.
#[derive(Debug, Default)]
pub struct QueueDelay {
    /// Increasing round trips, oldest first: the classic monotonic deque.
    window: VecDeque<(Instant, Duration)>,
}

impl QueueDelay {
    pub const WINDOW: Duration = Duration::from_secs(15);

    /// Add a round trip measured at `now`; returns the queueing delay.
    pub fn sample(&mut self, rtt: Duration, now: Instant) -> Duration {
        while self.window.back().is_some_and(|(_, r)| *r >= rtt) {
            self.window.pop_back();
        }
        self.window.push_back((now, rtt));
        while self
            .window
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > Self::WINDOW)
        {
            self.window.pop_front();
        }
        let min = self.window.front().map_or(rtt, |(_, r)| *r);
        rtt.saturating_sub(min)
    }
}

/// How often viewers acknowledge (see `viewer::client`); the relay and
/// the agent allow for this interval.
pub const ACK_INTERVAL: Duration = Duration::from_millis(100);

/// Allowance for acknowledgements being periodic, and for jitter.
const ACK_SLACK: Duration = Duration::from_millis(250);

/// Unacknowledged frames remembered per viewer.
const MAX_OUTSTANDING: usize = 1024;

/// One viewer's link delay, from its frame acknowledgements (viewers of
/// protocol 7+ send `FrameAck` every [`ACK_INTERVAL`] or so).
///
/// Each acknowledgement times a round trip: from when the frame arrived
/// from the agent (so time queued in the relay counts too) to when the
/// viewer said it had it. Above the recent minimum, that is queueing.
///
/// A viewer that stops acknowledging altogether (its link is jammed, or its
/// decoder has stalled) produces no samples at all, which must not read as
/// "healthy". So the delay is also at least the age of the oldest frame
/// still unacknowledged, less the usual round trip and the ack interval.
///
/// Used by the relay for relayed viewers and by the agent for viewers on
/// a direct path, where the agent itself is the sender.
#[derive(Debug, Default)]
pub struct ViewerDelay {
    queue: QueueDelay,
    /// Queueing delay from the latest acknowledgement.
    sampled: Duration,
    /// Round trip with empty queues; `None` until the first
    /// acknowledgement (a viewer that never acknowledges is not judged).
    base: Option<Duration>,
    /// Forwarded and not yet acknowledged: (seq, arrived from the agent).
    outstanding: VecDeque<(u64, Instant)>,
}

impl ViewerDelay {
    /// Frame `seq` was written to the viewer; `arrived` is when the relay
    /// received it from the agent (or when the agent sent it).
    pub fn forwarded(&mut self, seq: u64, arrived: Instant) {
        if self.outstanding.len() == MAX_OUTSTANDING {
            self.outstanding.pop_front();
        }
        self.outstanding.push_back((seq, arrived));
    }

    /// The viewer has every frame up to `seq`.
    pub fn acked(&mut self, seq: u64, now: Instant) {
        let mut newest = None;
        while let Some(&(s, arrived)) = self.outstanding.front() {
            if s > seq {
                break;
            }
            newest = Some((s, arrived));
            self.outstanding.pop_front();
        }
        if let Some((s, arrived)) = newest.filter(|(s, _)| *s == seq) {
            debug_assert_eq!(s, seq);
            let rtt = now.saturating_duration_since(arrived);
            self.sampled = self.queue.sample(rtt, now);
            self.base = Some(rtt - self.sampled);
        }
    }

    /// How far behind the viewer's link is right now.
    pub fn current(&self, now: Instant) -> Duration {
        let Some(base) = self.base else {
            return Duration::ZERO;
        };
        let stalled = self
            .outstanding
            .front()
            .map_or(Duration::ZERO, |(_, arrived)| {
                now.saturating_duration_since(*arrived)
                    .saturating_sub(base + ACK_SLACK)
            });
        self.sampled.max(stalled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{read_frame, write_frame};

    fn payload(n: u8) -> VideoPayload {
        VideoPayload {
            monitor: 1,
            pts_us: 33_333 * u64::from(n),
            width: 2560,
            height: 1440,
            h264: vec![0, 0, 0, 1, 0x65, n],
        }
    }

    #[tokio::test]
    async fn media_frames_round_trip_through_framing() {
        let frames: Vec<MediaFrame> = (0..3)
            .map(|n| payload(n).to_frame(n.into(), n == 0))
            .collect();
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).await.unwrap();
        }
        let mut reader = buf.as_slice();
        for f in &frames {
            let got: MediaFrame = read_frame(&mut reader).await.unwrap().unwrap();
            assert_eq!(&got, f);
            assert_eq!(got.video().unwrap(), payload(got.seq as u8));
        }
    }

    #[test]
    fn a_large_keyframe_fits_in_one_frame() {
        let big = VideoPayload {
            h264: vec![7; 4 * 1024 * 1024],
            ..payload(0)
        };
        let frame = big.to_frame(0, true);
        assert!(frame.payload.len() < crate::MAX_FRAME_LEN as usize);
    }

    #[test]
    fn server_visible_header_is_separate_from_payload() {
        // The relay needs seq and keyframe without decoding the payload.
        let frame = MediaFrame {
            seq: 9,
            keyframe: true,
            payload: b"ciphertext in phase 10".to_vec(),
        };
        let bytes = postcard::to_stdvec(&frame).unwrap();
        let back: MediaFrame = postcard::from_bytes(&bytes).unwrap();
        assert!(back.keyframe && back.seq == 9);
        assert!(back.video().is_err());
    }

    #[test]
    fn send_log_finds_recent_frames_and_forgets_old_ones() {
        let t0 = Instant::now();
        let mut log = SendLog::default();
        for seq in 0..(SendLog::CAPACITY as u64 + 10) {
            log.record(seq, t0 + Duration::from_millis(seq));
        }
        assert_eq!(log.sent_at(5), None, "evicted");
        assert_eq!(log.sent_at(500), Some(t0 + Duration::from_millis(500)));
        assert_eq!(log.sent_at(10_000), None);
        log.clear();
        assert_eq!(log.sent_at(500), None);
    }

    #[test]
    fn queue_delay_is_the_round_trip_above_the_recent_minimum() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut q = QueueDelay::default();
        assert_eq!(q.sample(ms(40), t0), ms(0));
        assert_eq!(q.sample(ms(30), t0 + ms(100)), ms(0));
        assert_eq!(q.sample(ms(250), t0 + ms(200)), ms(220));
        assert_eq!(q.sample(ms(35), t0 + ms(300)), ms(5));
        // The 30 ms minimum ages out of the window; 35 ms has not yet.
        let later = t0 + QueueDelay::WINDOW + ms(150);
        assert_eq!(q.sample(ms(80), later), ms(45));
        // Once every old sample has aged out, the slower path is the new
        // baseline rather than permanent "congestion".
        let much_later = later + QueueDelay::WINDOW + ms(1);
        assert_eq!(q.sample(ms(90), much_later), ms(0));
    }
}

#[cfg(test)]
mod viewer_delay_tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn prompt_acknowledgements_mean_no_delay() {
        let t0 = Instant::now();
        let mut v = ViewerDelay::default();
        for seq in 0..50u64 {
            let at = t0 + MS * (33 * seq as u32);
            v.forwarded(seq, at);
            v.acked(seq, at + 20 * MS);
        }
        assert_eq!(v.current(t0 + MS * 1700), Duration::ZERO);
    }

    #[test]
    fn slow_acknowledgements_show_the_queue() {
        let t0 = Instant::now();
        let mut v = ViewerDelay::default();
        v.forwarded(0, t0);
        v.acked(0, t0 + 20 * MS);
        // Frames 1-3 queue up behind a slow link; only 3 is acked (acks
        // cover everything before them).
        for seq in 1..=3u64 {
            v.forwarded(seq, t0 + MS * (100 * seq as u32));
        }
        v.acked(3, t0 + 820 * MS);
        assert_eq!(v.current(t0 + 820 * MS), 500 * MS);
    }

    #[test]
    fn a_viewer_that_stops_acknowledging_is_not_mistaken_for_a_fast_one() {
        let t0 = Instant::now();
        let mut v = ViewerDelay::default();
        v.forwarded(0, t0);
        v.acked(0, t0 + 20 * MS);
        v.forwarded(1, t0 + 100 * MS);
        v.forwarded(2, t0 + 133 * MS);
        // Within the ack interval: fine.
        assert_eq!(v.current(t0 + 300 * MS), Duration::ZERO);
        // Two seconds of silence: frame 1 is 1.9 s old.
        let now = t0 + 2000 * MS;
        assert_eq!(v.current(now), (1900 - 20 - 250) * MS);
    }

    #[test]
    fn a_viewer_that_never_acknowledges_is_not_judged() {
        let t0 = Instant::now();
        let mut v = ViewerDelay::default();
        v.forwarded(0, t0);
        assert_eq!(v.current(t0 + Duration::from_secs(10)), Duration::ZERO);
    }

    #[test]
    fn acknowledging_a_frame_that_was_skipped_still_clears_older_ones() {
        let t0 = Instant::now();
        let mut v = ViewerDelay::default();
        v.forwarded(0, t0);
        v.acked(0, t0 + 20 * MS);
        v.forwarded(1, t0 + 33 * MS);
        v.forwarded(5, t0 + 166 * MS);
        // The viewer acks 3 (it never got 2-4 from us; its count is what it
        // saw). Frame 1 is covered; no sample, since 3 was not forwarded.
        v.acked(3, t0 + 200 * MS);
        assert_eq!(v.outstanding.front().map(|f| f.0), Some(5));
        assert_eq!(v.current(t0 + 200 * MS), Duration::ZERO);
    }
}
