//! The viewer's single, ordered stream of video from two paths.
//!
//! While a session moves onto a direct path, the agent sends each frame
//! both ways until the viewer confirms the switch, so frames arrive twice
//! and in any interleaving: the direct path is usually faster, so frame
//! `n + 1` can arrive (direct) before frame `n` (relay). Later, if the
//! direct path drops, video resumes through the relay.
//!
//! [`Merger`] delivers each frame once, in sequence order:
//!
//! - a frame already delivered (or older) is a duplicate: dropped;
//! - the next frame in sequence, or a keyframe (decoding restarts there
//!   anyway), is delivered at once, with any held frames it unblocks;
//! - a frame after a gap is held for up to [`HOLD`] for the gap to fill
//!   from the other path; after that the gap is given up on (the decoder
//!   then asks for a keyframe, as after any loss).
//!
//! So the switch itself costs no keyframe and shows no glitch.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use protocol::media::MediaFrame;

/// Where a frame came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Relay,
    Direct,
}

/// How long a frame after a gap waits for the gap to fill.
pub const HOLD: Duration = Duration::from_millis(250);

/// Frames held at most; beyond this the gap is given up on at once.
const MAX_HELD: usize = 32;

#[derive(Debug, Default)]
pub struct Merger {
    /// Newest sequence number delivered.
    last: Option<u64>,
    held: BTreeMap<u64, (Instant, Source, MediaFrame)>,
}

impl Merger {
    /// A frame arrived; returns what to deliver now, in order.
    pub fn push(
        &mut self,
        frame: MediaFrame,
        source: Source,
        now: Instant,
    ) -> Vec<(Source, MediaFrame)> {
        let seq = frame.seq;
        if self.last.is_some_and(|last| seq <= last) || self.held.contains_key(&seq) {
            return Vec::new();
        }
        let in_order = self.last.is_none_or(|last| seq == last + 1);
        if in_order || frame.keyframe {
            // Anything held from before a keyframe is useless now.
            self.held.retain(|s, _| *s > seq);
            return self.deliver_from(seq, source, frame);
        }
        self.held.insert(seq, (now, source, frame));
        if self.held.len() > MAX_HELD {
            return self.give_up();
        }
        Vec::new()
    }

    /// Give up on a gap that has waited [`HOLD`].
    pub fn expire(&mut self, now: Instant) -> Vec<(Source, MediaFrame)> {
        match self.held.first_key_value() {
            Some((_, (since, _, _))) if now.saturating_duration_since(*since) >= HOLD => {
                self.give_up()
            }
            _ => Vec::new(),
        }
    }

    /// When [`Merger::expire`] next has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        self.held
            .first_key_value()
            .map(|(_, (since, _, _))| *since + HOLD)
    }

    /// Forget the gap: deliver from the oldest held frame on.
    fn give_up(&mut self) -> Vec<(Source, MediaFrame)> {
        match self.held.pop_first() {
            Some((seq, (_, source, frame))) => self.deliver_from(seq, source, frame),
            None => Vec::new(),
        }
    }

    fn deliver_from(
        &mut self,
        seq: u64,
        source: Source,
        frame: MediaFrame,
    ) -> Vec<(Source, MediaFrame)> {
        let mut out = vec![(source, frame)];
        let mut last = seq;
        while let Some((_, source, frame)) = self.held.remove(&(last + 1)) {
            out.push((source, frame));
            last += 1;
        }
        self.last = Some(last);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64, keyframe: bool) -> MediaFrame {
        MediaFrame {
            seq,
            keyframe,
            payload: vec![seq as u8],
        }
    }

    fn seqs(out: &[(Source, MediaFrame)]) -> Vec<u64> {
        out.iter().map(|(_, f)| f.seq).collect()
    }

    #[test]
    fn switching_paths_delivers_every_frame_once_in_order() {
        let t = Instant::now();
        let mut m = Merger::default();
        assert_eq!(seqs(&m.push(frame(0, true), Source::Relay, t)), [0]);
        assert_eq!(seqs(&m.push(frame(1, false), Source::Relay, t)), [1]);
        // The direct path starts at 3 and overtakes the relay's 2.
        assert_eq!(seqs(&m.push(frame(3, false), Source::Direct, t)), [0u64; 0]);
        assert_eq!(seqs(&m.push(frame(4, false), Source::Direct, t)), [0u64; 0]);
        let out = m.push(frame(2, false), Source::Relay, t);
        assert_eq!(seqs(&out), [2, 3, 4]);
        assert_eq!(out[1].0, Source::Direct);
        // The relay's copies of 3 and 4 are duplicates.
        assert_eq!(seqs(&m.push(frame(3, false), Source::Relay, t)), [0u64; 0]);
        assert_eq!(seqs(&m.push(frame(4, false), Source::Relay, t)), [0u64; 0]);
        assert_eq!(seqs(&m.push(frame(5, false), Source::Direct, t)), [5]);
        assert_eq!(m.deadline(), None);
    }

    #[test]
    fn a_gap_that_never_fills_is_given_up_after_the_hold() {
        let t = Instant::now();
        let mut m = Merger::default();
        m.push(frame(0, true), Source::Relay, t);
        assert_eq!(seqs(&m.push(frame(2, false), Source::Direct, t)), [0u64; 0]);
        assert_eq!(m.deadline(), Some(t + HOLD));
        assert_eq!(seqs(&m.expire(t + HOLD / 2)), [0u64; 0]);
        assert_eq!(seqs(&m.expire(t + HOLD)), [2]);
        // The missing frame turning up later is too late.
        assert_eq!(
            seqs(&m.push(frame(1, false), Source::Relay, t + HOLD)),
            [0u64; 0]
        );
    }

    #[test]
    fn a_keyframe_after_a_gap_is_delivered_at_once() {
        // E.g. the relay resuming after the direct path dropped: it starts
        // the viewer on a keyframe.
        let t = Instant::now();
        let mut m = Merger::default();
        m.push(frame(0, true), Source::Direct, t);
        m.push(frame(1, false), Source::Direct, t);
        assert_eq!(seqs(&m.push(frame(5, false), Source::Relay, t)), [0u64; 0]);
        assert_eq!(seqs(&m.push(frame(9, true), Source::Relay, t)), [9]);
        assert_eq!(m.deadline(), None, "frames before the keyframe dropped");
        assert_eq!(seqs(&m.push(frame(10, false), Source::Relay, t)), [10]);
    }

    #[test]
    fn too_many_held_frames_give_up_the_gap_early() {
        let t = Instant::now();
        let mut m = Merger::default();
        m.push(frame(0, true), Source::Relay, t);
        let mut delivered = Vec::new();
        for seq in 2..(2 + MAX_HELD as u64 + 1) {
            delivered.extend(seqs(&m.push(frame(seq, false), Source::Direct, t)));
        }
        assert_eq!(delivered.first(), Some(&2));
        assert_eq!(delivered.len(), MAX_HELD + 1);
    }
}
