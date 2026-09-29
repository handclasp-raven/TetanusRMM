//! Service side: connects the agent core's media link to whichever helper
//! is currently attached over the pipe.
//!
//! The helper can come and go (logon, logoff, crash, update) independently
//! of the server connection. The bridge remembers what the server wants
//! (which monitor, or nothing) and replays it to each newly attached helper,
//! so a stream survives a helper restart.

use std::sync::{Arc, Mutex};

use protocol::ipc::IpcMessage;
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::debug;

use crate::media::source::{MediaCommand, MediaEvent, MediaSourceEnd};

#[derive(Default)]
struct State {
    helper: Option<mpsc::UnboundedSender<IpcMessage>>,
    monitor: Option<u32>,
    /// A frame was dropped here; ask the helper for a keyframe (once).
    resync_requested: bool,
}

pub struct Bridge {
    state: Mutex<State>,
    events: mpsc::Sender<MediaEvent>,
}

impl Bridge {
    /// Start the bridge: consumes commands from the agent core.
    pub fn start(source: MediaSourceEnd) -> Arc<Self> {
        let MediaSourceEnd {
            mut commands,
            events,
        } = source;
        let bridge = Arc::new(Self {
            state: Mutex::new(State::default()),
            events,
        });
        let b = bridge.clone();
        tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                b.command(command);
            }
        });
        bridge
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn command(&self, command: MediaCommand) {
        let mut state = self.state();
        let message = match command {
            MediaCommand::ListMonitors => IpcMessage::ListMonitors,
            MediaCommand::Start { monitor } => {
                state.monitor = Some(monitor);
                IpcMessage::StartCapture { monitor }
            }
            MediaCommand::Stop => {
                state.monitor = None;
                IpcMessage::StopCapture
            }
            MediaCommand::ForceKeyframe => IpcMessage::ForceKeyframe,
        };
        if let Some(helper) = &state.helper {
            let _ = helper.send(message);
        } else {
            debug!(?message, "no helper attached; will replay on attach");
        }
    }

    /// A helper connected: route commands to it and replay the current state.
    pub fn attach(&self, helper: mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        let _ = helper.send(IpcMessage::ListMonitors);
        if let Some(monitor) = state.monitor {
            let _ = helper.send(IpcMessage::StartCapture { monitor });
        }
        state.helper = Some(helper);
    }

    pub fn detach(&self) {
        self.state().helper = None;
    }

    /// Something the helper sent.
    pub async fn from_helper(&self, message: IpcMessage) {
        match message {
            IpcMessage::Monitors(list) => {
                let _ = self.events.send(MediaEvent::Monitors(list)).await;
            }
            IpcMessage::Frame(frame) => {
                let keyframe = frame.keyframe;
                // After a drop, deltas would reference the missing frame and
                // corrupt the picture: skip them until a keyframe.
                if self.state().resync_requested && !keyframe {
                    return;
                }
                match self.events.try_send(MediaEvent::Frame(frame)) {
                    Ok(()) => {
                        tracing::trace!(keyframe, "frame forwarded to network");
                        if keyframe {
                            self.state().resync_requested = false;
                        }
                    }
                    // The network side is behind: drop, and resync on a keyframe.
                    Err(TrySendError::Full(_)) => {
                        tracing::debug!("network behind; frame dropped");
                        let mut state = self.state();
                        if !state.resync_requested {
                            state.resync_requested = true;
                            if let Some(helper) = &state.helper {
                                let _ = helper.send(IpcMessage::ForceKeyframe);
                            }
                        }
                    }
                    Err(TrySendError::Closed(_)) => {}
                }
            }
            _ => {}
        }
    }
}
