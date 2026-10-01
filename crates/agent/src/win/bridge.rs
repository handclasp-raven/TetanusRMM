//! Service side: connects the agent core's media and desktop links to
//! whichever helper is currently attached over the pipe.
//!
//! The helper can come and go (logon, logoff, crash, update) independently
//! of the server connection. The bridge remembers what the server wants
//! (which monitor, or nothing) and who is connected (the tray list), and
//! replays both (and any adaptive-bitrate target) to each newly attached
//! helper, so a stream and the tray survive a helper restart. A consent
//! prompt that cannot reach a helper, or whose helper goes away, is
//! answered `Unavailable`.
//!
//! Input goes to the input helper (SYSTEM, so it reaches elevated windows)
//! when one is attached, and to the session helper otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use protocol::consent::PromptAnswer;
use protocol::ipc::IpcMessage;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::interactive::{DesktopCommand, DesktopEnd, DesktopEvent};
use crate::media::source::{MediaCommand, MediaEvent, MediaSourceEnd};

#[derive(Default)]
struct State {
    helper: Option<mpsc::UnboundedSender<IpcMessage>>,
    /// The input helper, if attached.
    injector: Option<mpsc::UnboundedSender<IpcMessage>>,
    monitor: Option<u32>,
    /// Adaptive-bitrate target, if lowered (cleared by a stop).
    bitrate: Option<u32>,
    /// Frame rate, once set (kept across stops: the next stream starts at
    /// the rate the technicians asked for).
    frame_rate: Option<u32>,
    /// A frame was dropped here; ask the helper for a keyframe (once).
    resync_requested: bool,
    /// Who is connected, for the tray.
    technicians: Vec<String>,
    /// Consent prompts on screen, by request id.
    prompts: HashMap<u64, oneshot::Sender<PromptAnswer>>,
}

pub struct Bridge {
    state: Mutex<State>,
    events: mpsc::Sender<MediaEvent>,
    desktop_events: mpsc::UnboundedSender<DesktopEvent>,
}

impl Bridge {
    /// Start the bridge: consumes commands from the agent core.
    pub fn start(media: MediaSourceEnd, desktop: DesktopEnd) -> Arc<Self> {
        let MediaSourceEnd {
            mut commands,
            events,
        } = media;
        let DesktopEnd {
            commands: mut desktop_commands,
            events: desktop_events,
        } = desktop;
        let bridge = Arc::new(Self {
            state: Mutex::new(State::default()),
            events,
            desktop_events,
        });
        let b = bridge.clone();
        tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                b.command(command);
            }
        });
        let b = bridge.clone();
        tokio::spawn(async move {
            while let Some(command) = desktop_commands.recv().await {
                b.desktop_command(command);
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
                state.bitrate = None;
                IpcMessage::StopCapture
            }
            MediaCommand::ForceKeyframe => IpcMessage::ForceKeyframe,
            MediaCommand::SetBitrate(bps) => {
                state.bitrate = Some(bps);
                IpcMessage::SetBitrate { bps }
            }
            MediaCommand::SetFrameRate(fps) => {
                state.frame_rate = Some(fps);
                IpcMessage::SetFrameRate { fps }
            }
        };
        if let Some(helper) = &state.helper {
            let _ = helper.send(message);
        } else {
            debug!(?message, "no helper attached; will replay on attach");
        }
    }

    fn desktop_command(&self, command: DesktopCommand) {
        let mut state = self.state();
        let message = match command {
            DesktopCommand::Prompt {
                request_id,
                technician,
                timeout,
                reply,
            } => {
                if state.helper.is_none() {
                    warn!(request_id, "consent prompt needed but no helper is running");
                    let _ = reply.send(PromptAnswer::Unavailable);
                    return;
                }
                state.prompts.insert(request_id, reply);
                IpcMessage::ConsentPrompt {
                    request_id,
                    technician,
                    timeout_secs: timeout.as_secs().try_into().unwrap_or(u32::MAX),
                }
            }
            DesktopCommand::CancelPrompt { request_id } => {
                state.prompts.remove(&request_id);
                IpcMessage::ConsentCancel { request_id }
            }
            DesktopCommand::Toast { technician } => IpcMessage::Toast { technician },
            DesktopCommand::Technicians(list) => {
                state.technicians = list.clone();
                IpcMessage::Technicians(list)
            }
            DesktopCommand::Input(event) => {
                if let Some(injector) = &state.injector {
                    if injector.send(IpcMessage::Input(event)).is_ok() {
                        return;
                    }
                }
                IpcMessage::Input(event)
            }
            DesktopCommand::SetClipboard(data) => IpcMessage::SetClipboard(data),
        };
        if let Some(helper) = &state.helper {
            let _ = helper.send(message);
        }
    }

    /// A helper connected: route commands to it and replay the current state.
    pub fn attach(&self, helper: mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        let _ = helper.send(IpcMessage::ListMonitors);
        if let Some(bps) = state.bitrate {
            let _ = helper.send(IpcMessage::SetBitrate { bps });
        }
        if let Some(fps) = state.frame_rate {
            let _ = helper.send(IpcMessage::SetFrameRate { fps });
        }
        if let Some(monitor) = state.monitor {
            let _ = helper.send(IpcMessage::StartCapture { monitor });
        }
        let _ = helper.send(IpcMessage::Technicians(state.technicians.clone()));
        state.helper = Some(helper);
    }

    /// `helper` disconnected. Ignored if another helper has attached since.
    pub fn detach(&self, helper: &mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        if !state
            .helper
            .as_ref()
            .is_some_and(|h| h.same_channel(helper))
        {
            return;
        }
        state.helper = None;
        // Their prompts went with the helper.
        for (request_id, reply) in state.prompts.drain() {
            info!(request_id, "helper gone while a consent prompt was open");
            let _ = reply.send(PromptAnswer::Unavailable);
        }
    }

    /// An input helper connected: input goes to it from now on.
    pub fn attach_injector(&self, injector: mpsc::UnboundedSender<IpcMessage>) {
        self.state().injector = Some(injector);
    }

    /// `injector` disconnected: input falls back to the session helper.
    pub fn detach_injector(&self, injector: &mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        if state
            .injector
            .as_ref()
            .is_some_and(|i| i.same_channel(injector))
        {
            state.injector = None;
        }
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
            IpcMessage::ConsentAnswer { request_id, answer } => {
                match self.state().prompts.remove(&request_id) {
                    Some(reply) => {
                        let _ = reply.send(answer);
                    }
                    None => debug!(request_id, ?answer, "answer for a withdrawn prompt"),
                }
            }
            IpcMessage::Clipboard(data) => {
                let _ = self.desktop_events.send(DesktopEvent::Clipboard(data));
            }
            IpcMessage::KillSwitch => {
                let _ = self.desktop_events.send(DesktopEvent::KillSwitch);
            }
            other => debug!(?other, "ignoring message from helper"),
        }
    }
}
