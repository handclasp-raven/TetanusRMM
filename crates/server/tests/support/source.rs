//! A synthetic screen source for tests, standing in for the Windows helper
//! (DXGI + Media Foundation): two monitors, encoded with OpenH264.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::media::h264::{is_keyframe, ParamSets};
use agent::media::source::{MediaCommand, MediaEvent, MediaSourceEnd};
use openh264::encoder::Encoder;
use openh264::formats::YUVBuffer;
use protocol::ipc::EncodedFrame;
use protocol::media::MonitorInfo;
use tokio::sync::mpsc;

pub fn monitors() -> Vec<MonitorInfo> {
    vec![
        MonitorInfo {
            id: 0,
            name: r"\\.\DISPLAY1".into(),
            x: 0,
            y: 0,
            width: 320,
            height: 240,
            primary: true,
        },
        MonitorInfo {
            id: 1,
            name: r"\\.\DISPLAY2".into(),
            x: 320,
            y: 0,
            width: 256,
            height: 192,
            primary: false,
        },
    ]
}

/// Solid colour per monitor (as YUV), with a moving square so every frame
/// differs and the encoder produces real P-frames.
pub fn picture(monitor: u32, n: u64) -> YUVBuffer {
    let m = &monitors()[monitor as usize];
    let (w, h) = (m.width as usize, m.height as usize);
    // Monitor 0: red-ish. Monitor 1: blue-ish.
    let (y, u, v) = if monitor == 0 {
        (82u8, 90u8, 240u8)
    } else {
        (41, 240, 110)
    };
    let mut data = vec![y; w * h];
    let sq = (n as usize * 4) % (w - 16);
    for row in 0..16 {
        for col in 0..16 {
            data[row * w + sq + col] = 235;
        }
    }
    data.extend(std::iter::repeat_n(u, w * h / 4));
    data.extend(std::iter::repeat_n(v, w * h / 4));
    YUVBuffer::from_vec(data, w, h)
}

/// What the synthetic source was asked to do, and how much it encoded.
#[derive(Default)]
pub struct SourceLog {
    pub commands: Mutex<Vec<MediaCommand>>,
    pub encoded: AtomicU64,
}

impl SourceLog {
    pub fn commands(&self) -> Vec<MediaCommand> {
        self.commands.lock().unwrap().clone()
    }
    pub fn bitrates(&self) -> Vec<u32> {
        self.commands()
            .iter()
            .filter_map(|c| match c {
                MediaCommand::SetBitrate(bps) => Some(*bps),
                _ => None,
            })
            .collect()
    }

    pub fn starts(&self) -> usize {
        self.commands()
            .iter()
            .filter(|c| matches!(c, MediaCommand::Start { .. }))
            .count()
    }
}

/// Stands in for the Windows helper: obeys MediaCommands, encodes with OpenH264.
pub fn run_source(end: MediaSourceEnd, log: Arc<SourceLog>) {
    let MediaSourceEnd {
        mut commands,
        events,
    } = end;
    std::thread::spawn(move || {
        let mut active: Option<u32> = None;
        let mut encoder: Option<Encoder> = None;
        let mut params = ParamSets::default();
        let mut n = 0u64;
        loop {
            loop {
                match commands.try_recv() {
                    Ok(cmd) => {
                        log.commands.lock().unwrap().push(cmd.clone());
                        match cmd {
                            MediaCommand::ListMonitors => {
                                let _ = events.blocking_send(MediaEvent::Monitors(monitors()));
                            }
                            MediaCommand::Start { monitor } => {
                                active = Some(monitor);
                                encoder = Some(Encoder::new().unwrap()); // new size, fresh stream
                            }
                            MediaCommand::Stop => {
                                active = None;
                                encoder = None;
                            }
                            MediaCommand::ForceKeyframe => {
                                if let Some(e) = &mut encoder {
                                    e.force_intra_frame();
                                }
                            }
                            // Logged above; the test encoder's rate is fixed.
                            MediaCommand::SetBitrate(_) => {}
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            }
            if let (Some(monitor), Some(enc)) = (active, &mut encoder) {
                let data = params.fix_up(enc.encode(&picture(monitor, n)).unwrap().to_vec());
                let m = &monitors()[monitor as usize];
                let frame = EncodedFrame {
                    monitor,
                    keyframe: is_keyframe(&data),
                    pts_us: n * 33_333,
                    width: m.width,
                    height: m.height,
                    h264: data,
                };
                if events.try_send(MediaEvent::Frame(frame)).is_ok() {
                    log.encoded.fetch_add(1, Ordering::SeqCst);
                }
                n += 1;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    });
}
