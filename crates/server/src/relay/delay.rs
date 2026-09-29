//! One viewer's link delay, from its frame acknowledgements (viewers of
//! protocol 7+ send `FrameAck` every [`ACK_INTERVAL`] or so).
//!
//! Each acknowledgement times a round trip: from when the frame arrived
//! from the agent (so time queued in the relay counts too) to when the
//! viewer said it had it. Above the recent minimum, that is queueing.
//!
//! A viewer that stops acknowledging altogether (its link is jammed, or its
//! decoder has stalled) produces no samples at all, which must not read as
//! "healthy". So the delay is also at least the age of the oldest frame
//! still unacknowledged, less the usual round trip and the ack interval.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use protocol::media::QueueDelay;

/// How often viewers acknowledge (see `viewer::client`).
pub const ACK_INTERVAL: Duration = Duration::from_millis(100);

/// Allowance for acknowledgements being periodic, and for jitter.
const ACK_SLACK: Duration = Duration::from_millis(250);

/// Unacknowledged frames remembered per viewer.
const MAX_OUTSTANDING: usize = 1024;

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
    /// Frame `seq`, which arrived from the agent at `arrived`, was written
    /// to the viewer.
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
