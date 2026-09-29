//! The link between a frame source and the agent's server connection.
//!
//! The connection (in `crate::AgentSession`) turns the server's
//! `StartStream`/`StopStream`/`RequestKeyframe`/`ListMonitors` into
//! [`MediaCommand`]s, and turns [`MediaEvent`]s into `MonitorList` messages
//! and video frames. On Windows the source is the session helper, reached
//! over the named pipe; tests plug in a synthetic source.

use protocol::ipc::EncodedFrame;
use protocol::media::MonitorInfo;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaCommand {
    ListMonitors,
    Start {
        monitor: u32,
    },
    /// Stop capturing. The encoder also forgets any `SetBitrate`.
    Stop,
    ForceKeyframe,
    /// Encode at most this many bits per second (adaptive bitrate, see
    /// `media::rate`), for this stream and later ones until `Stop`.
    SetBitrate(u32),
}

#[derive(Debug, Clone)]
pub enum MediaEvent {
    Monitors(Vec<MonitorInfo>),
    Frame(EncodedFrame),
}

/// Frames buffered between the source and the network before the source is
/// told to drop frames. Small on purpose: stale video is worse than skipped
/// video.
pub const FRAME_BUFFER: usize = 8;

/// The agent's end of a media source.
pub struct MediaLink {
    pub commands: mpsc::UnboundedSender<MediaCommand>,
    pub events: Mutex<mpsc::Receiver<MediaEvent>>,
}

/// The source's end.
pub struct MediaSourceEnd {
    pub commands: mpsc::UnboundedReceiver<MediaCommand>,
    pub events: mpsc::Sender<MediaEvent>,
}

/// A connected pair.
pub fn media_channel() -> (MediaLink, MediaSourceEnd) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::channel(FRAME_BUFFER);
    (
        MediaLink {
            commands: cmd_tx,
            events: Mutex::new(ev_rx),
        },
        MediaSourceEnd {
            commands: cmd_rx,
            events: ev_tx,
        },
    )
}
