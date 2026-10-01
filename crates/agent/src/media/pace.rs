//! Per-frame bit budgets when the frame rate varies.
//!
//! Encoders' rate control budgets each frame as bitrate / the frame rate
//! they were told. Frames here come when the screen changes, anywhere from
//! one a minute to the frame-rate cap, so no fixed rate is right: told 60
//! while a busy screen yields 18 a second, every frame gets less than a
//! third of the bits it should, and moving content smears.
//!
//! So the encoder is always told [`ENCODER_FPS`], and its bitrate is set to
//! whatever gives each frame `target / measured frame rate`:
//! `target * ENCODER_FPS / measured` ([`encoder_bitrate`]). The measured
//! rate is frames captured over the last [`WINDOW`] ([`Pacer`]). A slow
//! screen gets bigger frames (at most [`MAX_BOOST`] times the nominal
//! budget); a fast one smaller ones; the stream's bitrate is the target
//! either way.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The frame rate encoders are told.
pub const ENCODER_FPS: u32 = 30;

/// Frame rate measured over this long.
pub const WINDOW: Duration = Duration::from_millis(500);

/// A frame on a slow screen gets at most this many times the budget it
/// would at [`ENCODER_FPS`].
pub const MAX_BOOST: f64 = 4.0;

/// Never ask an encoder for more than this (H.264 levels have limits).
const MAX_ENCODER_BITRATE: f64 = 80_000_000.0;

/// Re-tell the encoder only when its bitrate should move by this much...
const RETUNE_CHANGE: f64 = 0.1;
/// ...and at most this often.
const RETUNE_EVERY: Duration = Duration::from_millis(250);

/// The bitrate to give an encoder told [`ENCODER_FPS`] so that each frame
/// gets `target / fps` bits, with `fps` the measured frame rate (within
/// what [`MAX_BOOST`] and the cap allow).
pub fn encoder_bitrate(target: u32, fps: f64, cap: u32) -> u32 {
    let nominal = f64::from(ENCODER_FPS);
    let fps = fps.clamp(
        nominal / MAX_BOOST,
        f64::from(cap.max(1)).max(nominal / MAX_BOOST),
    );
    (f64::from(target) * nominal / fps).min(MAX_ENCODER_BITRATE) as u32
}

/// Measures the capture frame rate and keeps the encoder's bitrate in
/// step with it.
#[derive(Debug, Default)]
pub struct Pacer {
    frames: VecDeque<Instant>,
    /// Bitrate the encoder has now, and when it was set.
    applied: Option<(u32, Instant)>,
}

impl Pacer {
    /// The encoder was just created (or told) `bps`.
    pub fn applied(&mut self, bps: u32, now: Instant) {
        self.applied = Some((bps, now));
    }

    /// Re-tell the encoder at the next [`Pacer::retune`], whatever changed.
    pub fn invalidate(&mut self) {
        self.applied = None;
    }

    /// A frame was captured at `now`.
    pub fn frame(&mut self, now: Instant) {
        self.frames.push_back(now);
        self.prune(now);
    }

    /// Frames a second over the last [`WINDOW`].
    pub fn fps(&mut self, now: Instant) -> f64 {
        self.prune(now);
        self.frames.len() as f64 / WINDOW.as_secs_f64()
    }

    fn prune(&mut self, now: Instant) {
        while self
            .frames
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW)
        {
            self.frames.pop_front();
        }
    }

    /// The bitrate to tell the encoder now for `target` at a cap of `cap`
    /// frames a second, if it should be told (it moved enough, and not too
    /// soon after the last time).
    pub fn retune(&mut self, target: u32, cap: u32, now: Instant) -> Option<u32> {
        let bps = encoder_bitrate(target, self.fps(now), cap);
        if let Some((applied, at)) = self.applied {
            let change = (f64::from(bps) - f64::from(applied)).abs() / f64::from(applied.max(1));
            if change < RETUNE_CHANGE || now.saturating_duration_since(at) < RETUNE_EVERY {
                return None;
            }
        }
        self.applied = Some((bps, now));
        Some(bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn each_frame_gets_the_target_over_the_measured_rate() {
        let target = 3_000_000;
        // At the nominal rate: as is.
        assert_eq!(encoder_bitrate(target, 30.0, 60), target);
        // 60 real frames: each gets half as much.
        assert_eq!(encoder_bitrate(target, 60.0, 60), target / 2);
        // 18 real frames under a 120 cap: more each, not less.
        assert_eq!(encoder_bitrate(target, 18.0, 120), 5_000_000);
        // More frames than the cap allows is noise: the cap bounds it.
        assert_eq!(encoder_bitrate(target, 90.0, 60), target / 2);
        // An idle screen: boosted, but only so far.
        assert_eq!(encoder_bitrate(target, 1.0, 60), 12_000_000);
        assert_eq!(encoder_bitrate(target, 0.0, 15), 12_000_000);
        assert_eq!(encoder_bitrate(40_000_000, 1.0, 60), 80_000_000);
    }

    #[test]
    fn the_rate_is_measured_over_the_window() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        assert_eq!(pacer.fps(t0), 0.0);
        for i in 0..30 {
            pacer.frame(t0 + MS * (i * 1000 / 60));
        }
        let now = t0 + MS * 500;
        assert!(
            (58.0..=62.0).contains(&pacer.fps(now)),
            "{}",
            pacer.fps(now)
        );
        // The screen goes still: the rate decays to nothing.
        assert_eq!(pacer.fps(now + WINDOW + MS), 0.0);
    }

    #[test]
    fn the_encoder_is_retold_only_on_real_changes_and_not_too_often() {
        let t0 = Instant::now();
        let mut pacer = Pacer::default();
        pacer.applied(12_000_000, t0);
        // A steady 20 fps screen settles the bitrate at target * 30 / 20.
        let mut told = Vec::new();
        for i in 0..40u32 {
            let now = t0 + MS * (i * 50);
            pacer.frame(now);
            told.extend(pacer.retune(2_000_000, 60, now));
        }
        assert_eq!(told.last(), Some(&3_000_000), "{told:?}");
        assert!(told.len() <= 5, "settles quickly: {told:?}");
        let last = t0 + MS * 1950;
        assert_eq!(pacer.retune(2_000_000, 60, last), None, "no change");
        // A new target is passed on, once.
        told.clear();
        for i in 40..60u32 {
            let now = t0 + MS * (i * 50);
            pacer.frame(now);
            told.extend(pacer.retune(1_000_000, 60, now));
        }
        assert_eq!(told, [1_500_000]);
        // Invalidated: told at once, even with nothing changed.
        pacer.invalidate();
        let now = t0 + MS * 2950;
        assert_eq!(pacer.retune(1_000_000, 60, now), Some(1_500_000));
        assert_eq!(pacer.retune(1_000_000, 60, now), None);
    }
}
