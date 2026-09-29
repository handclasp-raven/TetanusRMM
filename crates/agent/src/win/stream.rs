//! The helper's capture worker: DXGI capture + Media Foundation encode on a
//! dedicated thread, driven by commands from the service.
//!
//! Idle (no viewers) it only waits for a command. Streaming, it waits for the
//! screen to change, encodes at most [`MAX_FPS`] frames a second, and sends
//! only frames where something changed (plus requested keyframes), so a
//! static screen costs almost nothing. If the pipe to the service is backed
//! up, frames are dropped rather than queued, and the next frame sent is a
//! keyframe so viewers resynchronise.
//!
//! The encoder starts at the resolution's full-quality bitrate
//! (`media::rate::max_bitrate`); the service lowers and raises it as the
//! network allows (`SetBitrate`, from the adaptive-bitrate controller).
//! The target survives monitor switches and is forgotten on `Stop`.

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use protocol::ipc::{EncodedFrame, IpcMessage};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{info, warn};

use super::capture::{self, CaptureError, Captured, Duplicator};
use super::encoder::H264Encoder;
use crate::media::rate::max_bitrate;

pub const MAX_FPS: u32 = 30;

/// How many times to re-feed an unchanged picture to coax out buffered output.
const MAX_OWED: u32 = 8;

/// Retry interval while the desktop cannot be duplicated (secure desktop,
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
}

/// Start the worker thread. Frames and monitor lists go to `out`.
pub fn spawn(out: mpsc::Sender<IpcMessage>) -> std_mpsc::Sender<WorkerCommand> {
    let (tx, rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || Worker { out, commands: rx }.run())
        .expect("spawn capture thread");
    tx
}

/// The bitrate for a `width` x `height` stream under `target`.
fn bitrate_for(width: u32, height: u32, target: Option<u32>) -> u32 {
    let max = max_bitrate(width, height);
    target.map_or(max, |t| t.min(max))
}

struct Stream {
    monitor: u32,
    duplicator: Duplicator,
    encoder: H264Encoder,
    started: Instant,
    force_keyframe: bool,
    stats: Stats,
    /// Frames given to the encoder that have not come out yet. While
    /// non-zero, the current picture is re-fed even if nothing changed, so
    /// an encoder that buffers frames still delivers the last change on an
    /// otherwise idle screen.
    owed: u32,
}

impl Stream {
    fn open(monitor: u32, target: Option<u32>) -> Result<Self, CaptureError> {
        let duplicator = Duplicator::new(monitor)?;
        let (w, h) = (duplicator.frame().width, duplicator.frame().height);
        let encoder = H264Encoder::new(w, h, MAX_FPS, bitrate_for(w, h, target))?;
        info!(
            monitor,
            width = w,
            height = h,
            encoder = %encoder.info().name,
            hardware = encoder.info().hardware,
            "capture started"
        );
        Ok(Self {
            monitor,
            duplicator,
            encoder,
            started: Instant::now(),
            force_keyframe: true,
            stats: Stats::new(),
            owed: 0,
        })
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
        }
    }

    fn maybe_log(&mut self, monitor: u32) {
        let elapsed = self.since.elapsed();
        if elapsed < Self::INTERVAL {
            return;
        }
        info!(
            monitor,
            captured = self.captured,
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
        let frame_interval = Duration::from_secs(1) / MAX_FPS;
        loop {
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
                    target = Some(bps);
                    if let Some(s) = &mut stream {
                        let (w, h) = s.encoder.size();
                        let bps = bitrate_for(w, h, target);
                        match s.encoder.set_bitrate(bps) {
                            Ok(()) => info!(bps, "encoder bitrate changed"),
                            // Some encoders only take it at creation; the
                            // next stream start applies it.
                            Err(e) => warn!(bps, "changing the encoder bitrate: {e}"),
                        }
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

            let s = match &mut stream {
                Some(s) => s,
                None => match Stream::open(monitor, target) {
                    Ok(s) => stream.insert(s),
                    Err(e) => {
                        warn!(monitor, "cannot start capture, retrying: {e}");
                        std::thread::sleep(RETRY);
                        continue;
                    }
                },
            };

            let tick = Instant::now();
            match s.duplicator.next_frame(frame_interval.as_millis() as u32) {
                Ok(Captured::Unchanged) if !s.force_keyframe && s.owed == 0 => {}
                Ok(_) => {
                    s.stats.captured += 1;
                    if !self.encode_and_send(s) {
                        // The pipe is gone; the helper is shutting down.
                        return;
                    }
                }
                Err(CaptureError::AccessLost) => {
                    // Desktop switch or display change. Re-enumerate (the
                    // monitor set may have changed) and start over.
                    info!("desktop duplication lost; restarting capture");
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
        let units = match s.encoder.encode(s.duplicator.frame(), pts_us, force) {
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
        assert_eq!(bitrate_for(1920, 1080, None), 4_000_000);
        assert_eq!(bitrate_for(1920, 1080, Some(1_500_000)), 1_500_000);
        assert_eq!(bitrate_for(640, 480, Some(3_000_000)), 1_000_000);
    }
}
