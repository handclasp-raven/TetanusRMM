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
