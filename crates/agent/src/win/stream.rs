//! A helper's capture worker: screen capture + Media Foundation encode on a
//! dedicated thread, driven by commands from the service. The session
//! helper has one for the user's desktop, the system helper one for the
//! secure desktop ([`Desktops`]).
//!
//! Idle (no viewers) it only waits for a command. Streaming, it waits for the
//! screen to change, encodes at most the frame rate it was given
//! (`SetFrameRate`, [`DEFAULT_FPS`] until then) frames a second, and sends
//! only frames where something changed (plus requested keyframes), so a
//! static screen costs almost nothing. If the pipe to the service is backed
//! up, frames are dropped rather than queued, and the next frame sent is a
//! keyframe so viewers resynchronise.
//!
//! The target bitrate starts at the resolution's full quality at that
//! frame rate (`media::rate::max_bitrate_at`); the service lowers and
//! raises it as the network allows (`SetBitrate`, from the adaptive-bitrate
//! controller). The target survives monitor switches and is forgotten on
//! `Stop`. What the encoder itself is told follows the frame rate actually
//! captured, so each frame gets its share of the target (`media::pace`).

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use protocol::ipc::{EncodedFrame, IpcMessage};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{info, warn};

use super::capture::{self, CaptureError, Captured, ScreenCapture};
use super::desktop::Follower;
use super::encoder::H264Encoder;
use crate::media::pace::{encoder_bitrate, Pacer, ENCODER_FPS};
use crate::media::rate::{max_bitrate_at, DEFAULT_FPS};

/// How many times to re-feed an unchanged picture to coax out buffered output.
const MAX_OWED: u32 = 8;

/// Retry interval while the desktop cannot be captured (secure desktop,
/// mode switch in progress).
const RETRY: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum WorkerCommand {
    ListMonitors,
    Start {
        monitor: u32,
    },
    Stop,
    ForceKeyframe,
    /// Adaptive bitrate: encode at most this many bits per second.
    SetBitrate(u32),
    /// Capture at most this many frames a second.
    SetFrameRate(u32),
}

/// Which desktops a worker captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktops {
    /// The one its process was started on: the session helper, on the
    /// user's desktop.
    Own,
    /// Whichever is on screen, the secure desktop included: the system
    /// helper. The worker's thread follows the input desktop (see
    /// `super::desktop`).
    Input,
}

/// Start the worker thread. Frames and monitor lists go to `out`.
pub fn spawn(out: mpsc::Sender<IpcMessage>, desktops: Desktops) -> std_mpsc::Sender<WorkerCommand> {
    let (tx, rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            Worker {
                out,
                commands: rx,
                desktops,
            }
            .run()
        })
        .expect("spawn capture thread");
    tx
}

/// The bitrate for a `width` x `height` stream at `fps` under `target`.
fn bitrate_for(width: u32, height: u32, fps: u32, target: Option<u32>) -> u32 {
    let max = max_bitrate_at(width, height, fps);
    target.map_or(max, |t| t.min(max))
}

struct Stream {
    monitor: u32,
    capture: ScreenCapture,
    encoder: H264Encoder,
    started: Instant,
    force_keyframe: bool,
    stats: Stats,
    /// Frames given to the encoder that have not come out yet. While
    /// non-zero, the current picture is re-fed even if nothing changed, so
    /// an encoder that buffers frames still delivers the last change on an
    /// otherwise idle screen.
    owed: u32,
    pacer: Pacer,
}

impl Stream {
    fn open(
        monitor: u32,
        fps: u32,
        target: Option<u32>,
        desktops: Desktops,
    ) -> Result<Self, CaptureError> {
        let capture = ScreenCapture::new(monitor, desktops == Desktops::Own)?;
        let (w, h) = (capture.frame().width, capture.frame().height);
        // No frame rate measured yet: the first frame (a keyframe) gets a
        // generous budget.
        let bitrate = encoder_bitrate(bitrate_for(w, h, fps, target), 0.0, fps);
        let encoder = H264Encoder::new(w, h, ENCODER_FPS, bitrate)?;
        let mut pacer = Pacer::default();
        pacer.applied(bitrate, Instant::now());
        info!(
            monitor,
            width = w,
            height = h,
            fps,
            encoder = %encoder.info().name,
            hardware = encoder.info().hardware,
            "capture started"
        );
        Ok(Self {
            monitor,
            capture,
            encoder,
            started: Instant::now(),
            force_keyframe: true,
            stats: Stats::new(),
            owed: 0,
            pacer,
        })
    }

    /// Keep the encoder's bitrate in step with the frame rate captured.
    fn pace(&mut self, fps: u32, target: Option<u32>) {
        let (w, h) = self.encoder.size();
        let target = bitrate_for(w, h, fps, target);
        let Some(bps) = self.pacer.retune(target, fps, Instant::now()) else {
            return;
        };
        if let Err(e) = self.encoder.set_bitrate(bps) {
            // Some encoders only take it at creation; the next stream start
            // applies it.
            warn!(bps, "changing the encoder bitrate: {e}");
        }
    }
}

/// Per-stream counters, logged every few seconds while streaming.
struct Stats {
    since: Instant,
    captured: u64,
    encoded: u64,
    sent: u64,
    dropped: u64,
    bytes: u64,
    /// Time spent getting changed frames (including waiting for them) and
    /// encoding them: which one limits the frame rate.
    capturing: Duration,
    /// Of `capturing`, copying and converting pixels (not waiting).
    copying: Duration,
    /// Changed regions and pixels copied.
    rects: u64,
    pixels: u64,
    encoding: Duration,
}

impl Stats {
    const INTERVAL: Duration = Duration::from_secs(10);

    fn new() -> Self {
        Self {
            since: Instant::now(),
            captured: 0,
            encoded: 0,
            sent: 0,
            dropped: 0,
            bytes: 0,
            capturing: Duration::ZERO,
            copying: Duration::ZERO,
            rects: 0,
            pixels: 0,
            encoding: Duration::ZERO,
        }
    }

    fn maybe_log(&mut self, monitor: u32) {
        let elapsed = self.since.elapsed();
        if elapsed < Self::INTERVAL {
            return;
        }
        let per_frame = |total: Duration| total.as_millis() as u64 / self.captured.max(1);
        info!(
            monitor,
            captured = self.captured,
            capture_ms = per_frame(self.capturing),
            copy_ms = per_frame(self.copying),
            rects = self.rects / self.captured.max(1),
            kpixels = self.pixels / self.captured.max(1) / 1000,
            encode_ms = per_frame(self.encoding),
            encoded = self.encoded,
            sent = self.sent,
            dropped = self.dropped,
            kbps = (self.bytes * 8 / 1000) / elapsed.as_secs().max(1),
            "stream stats (last {}s)",
            elapsed.as_secs()
        );
        *self = Self::new();
    }
}

struct Worker {
    out: mpsc::Sender<IpcMessage>,
    commands: std_mpsc::Receiver<WorkerCommand>,
    desktops: Desktops,
}

impl Worker {
    fn send_monitors(&self) {
        match capture::monitors() {
            Ok(list) => {
                let _ = self.out.blocking_send(IpcMessage::Monitors(list));
            }
            Err(e) => warn!("listing monitors: {e}"),
        }
    }

    fn run(self) {
        let mut stream: Option<Stream> = None;
        let mut wanted: Option<u32> = None;
        // Adaptive-bitrate target from the service, if it lowered it.
        let mut target: Option<u32> = None;
        let mut fps = DEFAULT_FPS;
        let mut follower = Follower::default();
        loop {
            let frame_interval = Duration::from_secs(1) / fps;
            // Idle: block until told what to do. Streaming: just drain.
            let command = if wanted.is_none() {
                match self.commands.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                }
            } else {
                match self.commands.try_recv() {
                    Ok(c) => Some(c),
                    Err(std_mpsc::TryRecvError::Empty) => None,
                    Err(std_mpsc::TryRecvError::Disconnected) => return,
                }
            };
            match command {
                Some(WorkerCommand::ListMonitors) => {
                    self.send_monitors();
                    continue;
                }
                Some(WorkerCommand::Start { monitor }) => {
                    if stream.as_ref().is_some_and(|s| s.monitor == monitor) {
                        stream.as_mut().expect("checked").force_keyframe = true;
                    } else {
                        stream = None;
                    }
                    wanted = Some(monitor);
                    continue;
                }
                Some(WorkerCommand::Stop) => {
                    if stream.take().is_some() {
                        info!("capture stopped");
                    }
                    wanted = None;
                    target = None;
                    continue;
                }
                Some(WorkerCommand::SetBitrate(bps)) => {
                    info!(bps, "target bitrate changed");
                    target = Some(bps);
                    if let Some(s) = &mut stream {
                        s.pacer.invalidate();
                        s.pace(fps, target);
                    }
                    continue;
                }
                Some(WorkerCommand::SetFrameRate(new)) => {
                    let new = new.max(1);
                    if new == fps {
                        continue;
                    }
                    fps = new;
                    info!(fps, "frame rate changed");
                    if let Some(s) = &mut stream {
                        s.pacer.invalidate();
                        s.pace(fps, target);
                    }
                    continue;
                }
                Some(WorkerCommand::ForceKeyframe) => {
                    if let Some(s) = &mut stream {
                        s.force_keyframe = true;
                    }
                    continue;
                }
                None => {}
            }
            let Some(monitor) = wanted else { continue };

            if stream.is_none() && self.desktops == Desktops::Input {
                // Nothing of the last capture is left on this thread, so
                // it can move to the desktop that is on screen now.
                match follower.follow() {
                    Ok(desktop) => tracing::debug!(desktop, "capturing the input desktop"),
                    Err(e) => {
                        warn!("cannot reach the input desktop, retrying: {e}");
                        std::thread::sleep(RETRY);
                        continue;
                    }
                }
            }
            let s = match &mut stream {
                Some(s) => s,
                None => match Stream::open(monitor, fps, target, self.desktops) {
                    Ok(s) => stream.insert(s),
                    Err(e) => {
                        warn!(monitor, "cannot start capture, retrying: {e}");
                        std::thread::sleep(RETRY);
                        continue;
                    }
                },
            };

            let tick = Instant::now();
            match s.capture.next_frame(frame_interval.as_millis() as u32) {
                Ok(Captured::Unchanged) if !s.force_keyframe && s.owed == 0 => {}
                Ok(captured) => {
                    if let Captured::Updated { pixels, rects } = captured {
                        s.stats.rects += rects as u64;
                        s.stats.pixels += pixels;
                    }
                    s.stats.captured += 1;
                    s.stats.capturing += tick.elapsed();
                    s.stats.copying += s.capture.last_copy;
                    s.pacer.frame(Instant::now());
                    s.pace(fps, target);
                    let encoding = Instant::now();
                    if !self.encode_and_send(s) {
                        // The pipe is gone; the helper is shutting down.
                        return;
                    }
                    s.stats.encoding += encoding.elapsed();
                }
                Err(CaptureError::AccessLost) => {
                    // Desktop switch or display change. Re-enumerate (the
                    // monitor set may have changed) and start over.
                    info!("screen capture lost; restarting it");
                    stream = None;
                    self.send_monitors();
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => {
                    warn!("capture failed, restarting: {e}");
                    stream = None;
                    std::thread::sleep(RETRY);
                    continue;
                }
            }
            if let Some(s) = &mut stream {
                s.stats.maybe_log(s.monitor);
            }
            if let Some(rest) = frame_interval.checked_sub(tick.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }

    /// Encode the current frame and hand it to the pipe. Returns false if
    /// the pipe is closed.
    fn encode_and_send(&self, s: &mut Stream) -> bool {
        let pts_us = s.started.elapsed().as_micros() as u64;
        let force = std::mem::take(&mut s.force_keyframe);
        let units = match s.encoder.encode(s.capture.frame(), pts_us, force) {
            Ok(units) => units,
            Err(e) => {
                warn!("encode failed: {e}");
                s.force_keyframe = true;
                return true;
            }
        };
        let (width, height) = s.encoder.size();
        // Give up re-feeding after a while (an encoder with a fixed delay
        // flushes as soon as the screen changes again anyway).
        s.owed = if units.is_empty() {
            (s.owed + 1).min(MAX_OWED)
        } else {
            0
        };
        if s.owed == MAX_OWED {
            s.owed = 0;
        }
        for au in units {
            s.stats.encoded += 1;
            s.stats.bytes += au.data.len() as u64;
            let frame = IpcMessage::Frame(EncodedFrame {
                monitor: s.monitor,
                keyframe: au.keyframe,
                pts_us,
                width,
                height,
                h264: au.data,
            });
            match self.out.try_send(frame) {
                Ok(()) => s.stats.sent += 1,
                // Backed up: drop this and the rest of the batch (they
                // depend on it), and make the next frame a keyframe.
                Err(TrySendError::Full(_)) => {
                    s.stats.dropped += 1;
                    s.force_keyframe = true;
                    break;
                }
                Err(TrySendError::Closed(_)) => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_lowers_but_never_raises_the_resolutions_bitrate() {
        assert_eq!(bitrate_for(1920, 1080, 30, None), 4_000_000);
        assert_eq!(bitrate_for(1920, 1080, 30, Some(1_500_000)), 1_500_000);
        assert_eq!(bitrate_for(640, 480, 30, Some(3_000_000)), 1_000_000);
        assert_eq!(
            bitrate_for(1920, 1080, 60, None),
            max_bitrate_at(1920, 1080, 60)
        );
    }
}
