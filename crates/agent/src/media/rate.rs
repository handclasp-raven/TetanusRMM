//! Adaptive bitrate: choose the encoder's target from how the video is
//! actually being delivered.
//!
//! The agent encodes once for every viewer, so the stream has to fit two
//! kinds of link: the agent's uplink to the server, and each viewer's
//! downlink from it. Both are judged by **queueing delay**, which rises the
//! moment a link is saturated (well before anything is lost):
//!
//! - uplink: the server reports the newest frame it has received
//!   (`StreamReport::seq`); the round trip from when this agent sent that
//!   frame, above the recent minimum, is time the frame spent queued;
//! - downlinks: the server does the same with viewers' acknowledgements
//!   and reports the worst (`StreamReport::viewer_delay_ms`).
//!
//! The controller is AIMD, like TCP: when either delay is above
//! [`CONGESTED`], cut the target (harder above [`SEVERE`], and never above
//! what the uplink is shown to deliver); when both have stayed under
//! [`CLEAR`] for a while, probe upwards by a few percent. The target stays
//! between [`MIN_BITRATE`] and the resolution's [`max_bitrate`].
//!
//! The ceiling also depends on the frame rate: more frames a second need
//! more bits, though not proportionally (consecutive frames differ less).
//!
//! **Automatic frame rate** (`FrameRate::Auto`) rides on the same signal.
//! The controller keeps a choice among [`AUTO_FRAME_RATES`], starting at
//! 30: once the stream has sat at full quality for that rate with the
//! links clear for a while, it tries the next faster rate; when the target
//! falls to under half of that rate's full quality (the link cannot carry
//! it), it drops to the next slower one. Trying faster again after a drop
//! waits longer each time, so a borderline link does not flap.
//!
//! Pure logic: the connection feeds it events and passes the targets it
//! returns to the encoder (`MediaCommand::SetBitrate`), and the frame rate
//! it streams at back in ([`RateController::set_frame_rate`]).

use std::time::{Duration, Instant};

use protocol::media::{QueueDelay, SendLog, StreamReport, AUTO_FRAME_RATES};

/// Never ask for less than this: below it the picture is not usable.
pub const MIN_BITRATE: u32 = 300_000;

/// Full-quality bitrate for a 1080p stream at [`DEFAULT_FPS`]; scaled by
/// area.
const BITS_PER_1080P: u32 = 4_000_000;

/// The frame rate streams start at, and the one [`max_bitrate`] is for.
pub const DEFAULT_FPS: u32 = 30;

/// Queueing above this means a link is saturated.
pub const CONGESTED: Duration = Duration::from_millis(250);
/// Far behind: back off hard.
pub const SEVERE: Duration = Duration::from_millis(1000);
/// Below this there is room to grow.
pub const CLEAR: Duration = Duration::from_millis(80);

const DECREASE: f64 = 0.8;
const SEVERE_DECREASE: f64 = 0.5;
const INCREASE: f64 = 1.08;
/// Cut at most this often, so a queue has time to drain before the next cut.
const DECREASE_EVERY: Duration = Duration::from_millis(500);
/// Grow at most this often, after this long without congestion.
const INCREASE_EVERY: Duration = Duration::from_secs(1);
/// Smaller changes are not worth reconfiguring the encoder for.
const MIN_CHANGE: f64 = 0.05;
/// Weight of the newest sample in the delivered-rate average.
const RATE_SMOOTHING: f64 = 0.3;

/// Automatic frame rate: below this share of a rate's full quality, the
/// link cannot carry it.
const AUTO_DOWN_SHARE: f64 = 0.5;
/// Keep an automatic rate at least this long before dropping it.
const AUTO_HOLD: Duration = Duration::from_secs(3);
/// Full quality and clear links this long before trying a faster rate...
const AUTO_UP_AFTER: Duration = Duration::from_secs(3);
/// ...doubling after each drop, up to this...
const AUTO_UP_AFTER_MAX: Duration = Duration::from_secs(60);
/// ...and back to the start once a rate has held this long.
const AUTO_STABLE: Duration = Duration::from_secs(60);

/// The full-quality bitrate for a `width` x `height` stream at
/// [`DEFAULT_FPS`]: 4 Mbit/s at 1080p, scaled by area, within 1-20 Mbit/s.
pub fn max_bitrate(width: u32, height: u32) -> u32 {
    let area = u64::from(width) * u64::from(height);
    ((u64::from(BITS_PER_1080P) * area) / (1920 * 1080)).clamp(1_000_000, 20_000_000) as u32
}

/// [`max_bitrate`] for `fps` frames a second: scaled by (fps / 30)^0.6
/// (0.5-2.5x), within 1-40 Mbit/s.
pub fn max_bitrate_at(width: u32, height: u32, fps: u32) -> u32 {
    let factor = (f64::from(fps.max(1)) / f64::from(DEFAULT_FPS))
        .powf(0.6)
        .clamp(0.5, 2.5);
    (f64::from(max_bitrate(width, height)) * factor).clamp(1_000_000.0, 40_000_000.0) as u32
}

/// Why the target moved, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Congested,
    Severe,
    Probe,
    /// The resolution or frame rate changed, and with it the ceiling.
    Ceiling,
    /// A new stream started while the target was below the ceiling.
    Restart,
}

/// A new encoder target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub bps: u32,
    pub reason: Reason,
    pub uplink_delay: Duration,
    pub viewer_delay: Duration,
}

#[derive(Debug)]
pub struct RateController {
    log: SendLog,
    uplink: QueueDelay,
    /// Current ceiling (from the resolution); 0 until the first frame.
    ceiling: u32,
    /// Current target; 0 until the first frame.
    target: u32,
    /// Last target given to the encoder. `None` after a restart: the
    /// encoder is back at its default (the ceiling).
    announced: Option<u32>,
    last_decrease: Option<Instant>,
    /// Since when delays have been clear (`None`: not clear).
    clear_since: Option<Instant>,
    last_increase: Option<Instant>,
    last_report: Option<(Instant, u64)>,
    /// Smoothed bits per second the server received.
    delivered: Option<f64>,
    last_delays: (Duration, Duration),
    /// Frame rate streamed at (the ceiling depends on it).
    fps: u32,
    /// Size of the last frame sent.
    size: Option<(u32, u32)>,
    auto: AutoRate,
}

/// The automatic frame-rate choice (see the module docs).
#[derive(Debug)]
struct AutoRate {
    fps: u32,
    changed: Option<Instant>,
    /// Since when the target has been at this rate's full quality with
    /// clear links.
    full_since: Option<Instant>,
    up_after: Duration,
}

impl Default for RateController {
    fn default() -> Self {
        Self::new()
    }
}

impl RateController {
    pub fn new() -> Self {
        Self {
            log: SendLog::default(),
            uplink: QueueDelay::default(),
            ceiling: 0,
            target: 0,
            announced: None,
            last_decrease: None,
            clear_since: None,
            last_increase: None,
            last_report: None,
            delivered: None,
            last_delays: (Duration::ZERO, Duration::ZERO),
            fps: DEFAULT_FPS,
            size: None,
            auto: AutoRate {
                fps: DEFAULT_FPS,
                changed: None,
                full_since: None,
                up_after: AUTO_UP_AFTER,
            },
        }
    }

    /// What `FrameRate::Auto` means right now.
    pub fn auto_fps(&self) -> u32 {
        self.auto.fps
    }

    /// The stream now runs at `fps` frames a second: move the ceiling. A
    /// target at full quality stays at full quality.
    pub fn set_frame_rate(&mut self, fps: u32) -> Option<Change> {
        if fps == self.fps {
            return None;
        }
        self.fps = fps;
        let (w, h) = self.size?;
        let old = self.ceiling;
        self.ceiling = max_bitrate_at(w, h, fps);
        if self.target == 0 {
            return None;
        }
        if self.target >= old || self.target > self.ceiling {
            self.target = self.ceiling;
        }
        self.announce(Reason::Ceiling)
    }

    /// The current target, once a frame has been seen.
    pub fn target(&self) -> Option<u32> {
        (self.target > 0).then_some(self.target)
    }

    /// The stream stopped (the encoder forgets its target): forget sent
    /// frames, and re-announce the target with the next frame.
    pub fn stream_restarted(&mut self) {
        self.log.clear();
        self.last_report = None;
        self.announced = None;
    }

    /// A frame of `width` x `height` went out as `seq`.
    pub fn on_sent(&mut self, seq: u64, width: u32, height: u32, now: Instant) -> Option<Change> {
        self.log.record(seq, now);
        self.size = Some((width, height));
        let ceiling = max_bitrate_at(width, height, self.fps);
        let mut reason = Reason::Restart;
        if ceiling != self.ceiling {
            self.ceiling = ceiling;
            if self.target == 0 {
                self.target = ceiling;
                self.announced = Some(ceiling);
            } else if self.target > ceiling {
                self.target = ceiling;
                reason = Reason::Ceiling;
            }
        }
        // A restarted encoder is at the ceiling; tell it otherwise.
        if self.announced.is_none() && self.target == self.ceiling {
            self.announced = Some(self.target);
        }
        self.announce(reason)
    }

    /// The server reported delivery.
    pub fn on_report(&mut self, report: &StreamReport, now: Instant) -> Option<Change> {
        let change = self.adjust(report, now);
        if self.target > 0 {
            let (uplink, viewers) = self.last_delays;
            self.update_auto(uplink.max(viewers), now);
        }
        change
    }

    /// Pick the automatic frame rate from how the target sits against each
    /// rate's full quality.
    fn update_auto(&mut self, delay: Duration, now: Instant) {
        let Some((w, h)) = self.size else { return };
        let auto = &mut self.auto;
        let full = max_bitrate_at(w, h, auto.fps);
        let held = auto
            .changed
            .map_or(Duration::MAX, |t| now.saturating_duration_since(t));
        let position = AUTO_FRAME_RATES.iter().position(|&r| r == auto.fps);
        if f64::from(self.target) < f64::from(full) * AUTO_DOWN_SHARE {
            auto.full_since = None;
            let slower = position.and_then(|i| AUTO_FRAME_RATES.get(i + 1));
            if let (Some(&slower), true) = (slower, held >= AUTO_HOLD) {
                auto.up_after = if held >= AUTO_STABLE {
                    AUTO_UP_AFTER
                } else {
                    (auto.up_after * 2).min(AUTO_UP_AFTER_MAX)
                };
                auto.fps = slower;
                auto.changed = Some(now);
            }
            return;
        }
        if self.target < full || delay >= CLEAR {
            auto.full_since = None;
            return;
        }
        let since = *auto.full_since.get_or_insert(now);
        let faster = position
            .and_then(|i| i.checked_sub(1))
            .map(|i| AUTO_FRAME_RATES[i]);
        if let Some(faster) = faster {
            if now.saturating_duration_since(since) >= auto.up_after && held >= auto.up_after {
                auto.fps = faster;
                auto.changed = Some(now);
                auto.full_since = None;
            }
        }
    }

    fn adjust(&mut self, report: &StreamReport, now: Instant) -> Option<Change> {
        if self.target == 0 {
            return None;
        }
        let uplink = match self.log.sent_at(report.seq) {
            Some(sent) => self.uplink.sample(now.saturating_duration_since(sent), now),
            None => Duration::ZERO,
        };
        let viewers = Duration::from_millis(report.viewer_delay_ms.into());
        self.last_delays = (uplink, viewers);

        if let Some((then, bytes)) = self.last_report {
            let secs = now.saturating_duration_since(then).as_secs_f64();
            if secs > 0.05 && report.bytes >= bytes {
                let rate = (report.bytes - bytes) as f64 * 8.0 / secs;
                self.delivered = Some(match self.delivered {
                    Some(avg) => avg + RATE_SMOOTHING * (rate - avg),
                    None => rate,
                });
            }
        }
        self.last_report = Some((now, report.bytes));

        let delay = uplink.max(viewers);
        if delay >= CONGESTED {
            self.clear_since = None;
            if self
                .last_decrease
                .is_some_and(|t| now.saturating_duration_since(t) < DECREASE_EVERY)
            {
                return None;
            }
            let severe = delay >= SEVERE;
            let mut next = f64::from(self.target) * if severe { SEVERE_DECREASE } else { DECREASE };
            // A saturated uplink delivers about its capacity: go below it.
            if uplink >= CONGESTED {
                if let Some(delivered) = self.delivered {
                    next = next.min(delivered * 0.9);
                }
            }
            self.target = (next as u32).clamp(MIN_BITRATE, self.ceiling);
            self.last_decrease = Some(now);
            return self.announce(if severe {
                Reason::Severe
            } else {
                Reason::Congested
            });
        }
        if delay < CLEAR {
            let since = *self.clear_since.get_or_insert(now);
            let waited = now.saturating_duration_since(since) >= INCREASE_EVERY
                && self
                    .last_increase
                    .is_none_or(|t| now.saturating_duration_since(t) >= INCREASE_EVERY);
            if waited && self.target < self.ceiling {
                self.target =
                    ((f64::from(self.target) * INCREASE) as u32).clamp(MIN_BITRATE, self.ceiling);
                self.last_increase = Some(now);
                return self.announce(Reason::Probe);
            }
        } else {
            self.clear_since = None;
        }
        None
    }

    fn announce(&mut self, reason: Reason) -> Option<Change> {
        let target = self.target;
        let worth_it = match self.announced {
            None => true,
            Some(prev) => {
                // Always reach the bounds exactly; otherwise skip tiny steps.
                (target == self.ceiling || target == MIN_BITRATE) && target != prev
                    || (f64::from(target) - f64::from(prev)).abs() / f64::from(prev) >= MIN_CHANGE
            }
        };
        if !worth_it || target == 0 {
            return None;
        }
        self.announced = Some(target);
        Some(Change {
            bps: target,
            reason,
            uplink_delay: self.last_delays.0,
            viewer_delay: self.last_delays.1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const W: u32 = 1920;
    const H: u32 = 1080;

    /// Drive a controller over a simulated link: frames every 33 ms,
    /// reports every 200 ms with the given round trip and viewer delay.
    struct Sim {
        rate: RateController,
        now: Instant,
        seq: u64,
        bytes: u64,
        changes: Vec<Change>,
        /// Stream at the automatic frame rate, as the agent does for
        /// `FrameRate::Auto`.
        follow_auto: bool,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                rate: RateController::new(),
                now: Instant::now(),
                seq: 0,
                bytes: 0,
                changes: Vec::new(),
                follow_auto: false,
            }
        }

        /// Run for `duration`; the server receives `rtt` after each send
        /// and delivers `kbps` of payload.
        fn run(&mut self, duration: Duration, rtt: Duration, viewer_ms: u32, kbps: u64) {
            let end = self.now + duration;
            let mut next_report = self.now + 200 * MS;
            while self.now < end {
                self.now += 33 * MS;
                self.seq += 1;
                self.changes
                    .extend(self.rate.on_sent(self.seq, W, H, self.now - rtt));
                self.bytes += kbps * 1000 / 8 * 33 / 1000;
                if self.now >= next_report {
                    next_report += 200 * MS;
                    let report = StreamReport {
                        seq: self.seq,
                        bytes: self.bytes,
                        viewer_delay_ms: viewer_ms,
                    };
                    self.changes.extend(self.rate.on_report(&report, self.now));
                    if self.follow_auto {
                        let fps = self.rate.auto_fps();
                        self.changes.extend(self.rate.set_frame_rate(fps));
                    }
                }
            }
        }

        fn target(&self) -> u32 {
            self.rate.target().unwrap()
        }
    }

    #[test]
    fn max_bitrate_scales_with_area_within_bounds() {
        assert_eq!(max_bitrate(1920, 1080), 4_000_000);
        assert_eq!(max_bitrate(2560, 1440), 7_111_111);
        assert_eq!(max_bitrate(640, 480), 1_000_000);
        assert_eq!(max_bitrate(7680, 4320), 20_000_000);
    }

    #[test]
    fn a_healthy_link_stays_at_full_quality_without_reconfiguring() {
        let mut sim = Sim::new();
        sim.run(Duration::from_secs(10), 20 * MS, 10, 3000);
        assert_eq!(sim.target(), max_bitrate(W, H));
        assert!(sim.changes.is_empty(), "{:?}", sim.changes);
    }

    #[test]
    fn a_saturated_uplink_cuts_below_what_it_delivers() {
        let mut sim = Sim::new();
        sim.run(Duration::from_secs(3), 20 * MS, 0, 3000);
        // The uplink only manages 1.5 Mbit/s: frames queue for 600 ms.
        sim.run(Duration::from_secs(2), 620 * MS, 0, 1500);
        let first = sim.changes[0];
        assert_eq!(first.reason, Reason::Congested);
        assert!(first.uplink_delay >= CONGESTED, "{first:?}");
        assert!(sim.target() <= 1_500_000 * 9 / 10 + 1, "{}", sim.target());
        // Cuts are spaced out, not one per report.
        assert!(sim.changes.len() <= 4, "{:?}", sim.changes);
    }

    #[test]
    fn a_slow_viewer_cuts_the_shared_stream_and_severe_delay_halves_it() {
        let mut sim = Sim::new();
        sim.run(Duration::from_secs(2), 20 * MS, 0, 3000);
        sim.run(300 * MS, 20 * MS, 400, 3000);
        assert_eq!(sim.changes.last().unwrap().reason, Reason::Congested);
        assert_eq!(sim.target(), 3_200_000);
        sim.run(600 * MS, 20 * MS, 1500, 3000);
        assert_eq!(sim.changes.last().unwrap().reason, Reason::Severe);
        assert_eq!(sim.target(), 1_600_000);
    }

    #[test]
    fn the_target_never_leaves_its_bounds_and_recovers_when_the_link_clears() {
        let mut sim = Sim::new();
        sim.run(Duration::from_secs(1), 20 * MS, 0, 3000);
        sim.run(Duration::from_secs(20), 20 * MS, 5000, 100);
        assert_eq!(sim.target(), MIN_BITRATE);
        assert_eq!(sim.changes.last().unwrap().bps, MIN_BITRATE);

        // Congestion over: probe back up, a step at a time, to the ceiling.
        sim.run(Duration::from_secs(60), 20 * MS, 10, 3000);
        assert_eq!(sim.target(), max_bitrate(W, H));
        let probes = sim
            .changes
            .iter()
            .filter(|c| c.reason == Reason::Probe)
            .count();
        assert!(probes >= 10, "gradual: {probes} steps");
        assert_eq!(sim.changes.last().unwrap().bps, max_bitrate(W, H));
    }

    #[test]
    fn a_restarted_stream_is_told_the_current_target_and_resolution_caps_it() {
        let mut sim = Sim::new();
        sim.run(Duration::from_secs(1), 20 * MS, 0, 3000);
        sim.run(Duration::from_secs(1), 20 * MS, 600, 3000);
        let learned = sim.target();
        assert!(learned < max_bitrate(W, H));
        sim.changes.clear();

        sim.rate.stream_restarted();
        sim.seq += 1;
        let change = sim.rate.on_sent(sim.seq, W, H, sim.now).unwrap();
        assert_eq!((change.bps, change.reason), (learned, Reason::Restart));

        // Switching to a small monitor lowers the ceiling below the target.
        sim.seq += 1;
        let change = sim.rate.on_sent(sim.seq, 640, 480, sim.now).unwrap();
        assert_eq!((change.bps, change.reason), (1_000_000, Reason::Ceiling));
    }

    #[test]
    fn reports_before_any_frame_or_for_unknown_frames_are_harmless() {
        let mut rate = RateController::new();
        let report = StreamReport {
            seq: 5,
            bytes: 0,
            viewer_delay_ms: 0,
        };
        assert_eq!(rate.on_report(&report, Instant::now()), None);
        assert_eq!(rate.target(), None);
        assert_eq!(rate.on_sent(1, W, H, Instant::now()), None);
        assert_eq!(rate.on_report(&report, Instant::now()), None);
    }

    #[test]
    fn the_ceiling_grows_with_the_frame_rate_but_less_than_proportionally() {
        assert_eq!(max_bitrate_at(1920, 1080, 30), 4_000_000);
        let at60 = max_bitrate_at(1920, 1080, 60);
        assert!((6_000_000..6_100_000).contains(&at60), "{at60}");
        assert_eq!(max_bitrate_at(1920, 1080, 240), 10_000_000);
        assert_eq!(max_bitrate_at(640, 480, 15), 1_000_000);
        assert_eq!(max_bitrate_at(7680, 4320, 120), 40_000_000);
    }

    #[test]
    fn a_new_frame_rate_moves_the_ceiling_and_keeps_full_quality_full() {
        let mut sim = Sim::new();
        sim.run(Duration::from_secs(1), 20 * MS, 0, 3000);
        let change = sim.rate.set_frame_rate(60).unwrap();
        assert_eq!(change.reason, Reason::Ceiling);
        assert_eq!(change.bps, max_bitrate_at(W, H, 60));

        // Congested at 60: then slowing down only caps the lower target.
        sim.run(Duration::from_secs(1), 20 * MS, 600, 3000);
        let learned = sim.target();
        assert!(learned < max_bitrate_at(W, H, 60));
        sim.changes.clear();
        let change = sim.rate.set_frame_rate(15);
        assert_eq!(
            sim.target(),
            learned.min(max_bitrate_at(W, H, 15)),
            "{change:?}"
        );
        assert_eq!(
            sim.rate.set_frame_rate(15),
            None,
            "no change, no announcement"
        );
    }

    #[test]
    fn auto_speeds_up_on_a_clear_link_and_stays_there() {
        let mut sim = Sim::new();
        sim.follow_auto = true;
        assert_eq!(sim.rate.auto_fps(), 30);
        sim.run(Duration::from_secs(2), 20 * MS, 10, 3000);
        assert_eq!(sim.rate.auto_fps(), 30, "not straight away");
        sim.run(Duration::from_secs(3), 20 * MS, 10, 3000);
        assert_eq!(sim.rate.auto_fps(), 60);
        assert_eq!(sim.target(), max_bitrate_at(W, H, 60));
        sim.run(Duration::from_secs(30), 20 * MS, 10, 3000);
        assert_eq!(sim.rate.auto_fps(), 60, "60 is the fastest automatic rate");
    }

    #[test]
    fn auto_slows_down_when_the_link_cannot_carry_it_and_backs_off_retrying() {
        let mut sim = Sim::new();
        sim.follow_auto = true;
        sim.run(Duration::from_secs(5), 20 * MS, 10, 3000);
        assert_eq!(sim.rate.auto_fps(), 60);

        // A slow viewer: the target collapses, and the rate with it.
        sim.run(Duration::from_secs(6), 20 * MS, 1500, 300);
        assert_eq!(sim.rate.auto_fps(), 15);

        // Clear again: faster rates come back, but not at once.
        sim.run(Duration::from_secs(4), 20 * MS, 10, 3000);
        assert_eq!(
            sim.rate.auto_fps(),
            15,
            "retrying waits longer after a drop"
        );
        sim.run(Duration::from_secs(120), 20 * MS, 10, 3000);
        assert_eq!(sim.rate.auto_fps(), 60);
    }
}
