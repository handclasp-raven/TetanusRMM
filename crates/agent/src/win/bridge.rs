//! Service side: connects the agent core's media and desktop links to
//! whichever helpers are currently attached over the pipe.
//!
//! The helpers can come and go (logon, logoff, crash, update) independently
//! of the server connection. The bridge remembers what the server wants
//! (which monitor, or nothing) and who is connected (the tray list), and
//! replays both (and any adaptive-bitrate target) to each newly attached
//! helper, so a stream and the tray survive a helper restart. A consent
//! prompt that cannot reach a helper, or whose helper goes away, is
//! answered `Unavailable`.
//!
//! One helper at a time captures the screen: the session helper on the
//! user's desktop, the system helper on the secure desktop (the logon
//! screen, the lock screen, UAC prompts) and when nobody is logged on (see
//! [`capturer`]). The system helper reports which desktop is showing, and
//! the stream moves between them as it changes; each starts with a
//! keyframe, so viewers follow.
//!
//! The password a user lends the technicians (see `protocol::credential`)
//! is kept here, in the service, encrypted in memory (see `super::secret`):
//! out of reach of the user's own processes, and still there when a helper
//! restarts. The session helper asks for it; it is typed by whichever
//! helper takes input.
//!
//! Input goes to the system helper (SYSTEM, so it reaches elevated windows
//! and the secure desktop) when one is attached, and to the session helper
//! otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use protocol::brand::Branding;
use protocol::consent::PromptAnswer;
use protocol::credential::CredentialEvent;
use protocol::ipc::{IpcMessage, Secret};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::sas;
use super::secret::ProtectedSecret;
use crate::interactive::{DesktopCommand, DesktopEnd, DesktopEvent};
use crate::media::source::{MediaCommand, MediaEvent, MediaSourceEnd};
use crate::session::{capturer, HelperRole};

#[derive(Default)]
struct State {
    /// The session helper, if attached.
    helper: Option<mpsc::UnboundedSender<IpcMessage>>,
    /// The system helper, if attached.
    system: Option<mpsc::UnboundedSender<IpcMessage>>,
    /// The secure desktop is showing (says the system helper).
    secure: bool,
    /// The helper capturing the screen: the only one sent capture
    /// commands, and the only one whose frames are passed on.
    capturer: Option<HelperRole>,
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
    /// The company's branding, once the server has said (`Some(None)`:
    /// TetanusRMM's own). A helper that attaches later is told.
    branding: Option<Option<Branding>>,
    /// Consent prompts on screen, by request id.
    prompts: HashMap<u64, oneshot::Sender<PromptAnswer>>,
    /// The password the user lent the technicians, if any.
    credential: Option<ProtectedSecret>,
    /// The prompt for one, while it is on screen.
    credential_prompt: Option<CredentialPrompt>,
    /// Request ids of password prompts so far.
    credential_prompts: u64,
}

/// The password prompt on screen, and everyone waiting on it.
struct CredentialPrompt {
    request_id: u64,
    waiting: Vec<oneshot::Sender<CredentialEvent>>,
}

impl CredentialPrompt {
    fn answer(self, event: CredentialEvent) {
        for reply in self.waiting {
            let _ = reply.send(event);
        }
    }
}

impl State {
    fn helper(&self, role: HelperRole) -> Option<&mpsc::UnboundedSender<IpcMessage>> {
        match role {
            HelperRole::User => self.helper.as_ref(),
            HelperRole::System => self.system.as_ref(),
        }
    }

    /// Have `text` typed where the keyboard focus is, like any other
    /// input: by the system helper when there is one, so it reaches the
    /// lock screen and UAC. `false` if no helper is there to type it.
    fn type_text(&self, text: Secret) -> bool {
        let message = IpcMessage::TypeText(text);
        let message = match &self.system {
            Some(system) => system.send(message).err().map(|e| e.0),
            None => Some(message),
        };
        match (message, &self.helper) {
            (None, _) => true,
            (Some(message), Some(helper)) => helper.send(message).is_ok(),
            (Some(_), None) => false,
        }
    }

    /// The helper capturing the screen, if any can.
    fn capturing(&self) -> Option<&mpsc::UnboundedSender<IpcMessage>> {
        self.capturer.and_then(|role| self.helper(role))
    }

    /// A helper in `role` attached. If that role was capturing, this is
    /// its replacement and knows nothing yet.
    fn attached(&mut self, role: HelperRole) {
        if self.capturer == Some(role) {
            self.capturer = None;
        }
        self.assign();
    }

    /// Work out again which helper captures. If that changed, stop the
    /// one that was and bring the other up to date with what the server
    /// wants.
    fn assign(&mut self) {
        let next = capturer(self.helper.is_some(), self.system.is_some(), self.secure);
        if next == self.capturer {
            return;
        }
        if let Some(old) = self.capturing() {
            let _ = old.send(IpcMessage::StopCapture);
        }
        info!(from = ?self.capturer, to = ?next, "screen capture handed over");
        self.capturer = next;
        let Some(helper) = self.capturing() else {
            return;
        };
        let _ = helper.send(IpcMessage::ListMonitors);
        if let Some(bps) = self.bitrate {
            let _ = helper.send(IpcMessage::SetBitrate { bps });
        }
        if let Some(fps) = self.frame_rate {
            let _ = helper.send(IpcMessage::SetFrameRate { fps });
        }
        if let Some(monitor) = self.monitor {
            let _ = helper.send(IpcMessage::StartCapture { monitor });
        }
    }
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
        if let Some(helper) = state.capturing() {
            let _ = helper.send(message);
        } else {
            debug!(?message, "no helper can capture; will replay when one can");
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
                if let Some(system) = &state.system {
                    if system.send(IpcMessage::Input(event)).is_ok() {
                        return;
                    }
                }
                IpcMessage::Input(event)
            }
            DesktopCommand::SetClipboard(data) => IpcMessage::SetClipboard(data),
            DesktopCommand::Branding(branding) => {
                state.branding = Some(branding.clone());
                IpcMessage::Branding(branding)
            }
            DesktopCommand::SecureAttention => {
                // Not a helper's job: only a service may (see `sas`). It
                // touches the registry, so not on this thread.
                std::thread::spawn(sas::send);
                return;
            }
            DesktopCommand::CredentialPrompt { technician, reply } => {
                // A second technician asking joins the prompt on screen.
                if let Some(prompt) = &mut state.credential_prompt {
                    prompt.waiting.push(reply);
                    return;
                }
                // The user's helper shows it, on the user's desktop: not
                // while that is locked or hidden by a UAC prompt.
                if state.helper.is_none() || state.secure {
                    warn!("password prompt needed but nobody is at the desktop");
                    let _ = reply.send(CredentialEvent::Unavailable);
                    return;
                }
                state.credential_prompts += 1;
                let request_id = state.credential_prompts;
                state.credential_prompt = Some(CredentialPrompt {
                    request_id,
                    waiting: vec![reply],
                });
                IpcMessage::CredentialPrompt {
                    request_id,
                    technician,
                }
            }
            DesktopCommand::CredentialType { reply } => {
                let text =
                    match state.credential.as_mut().map(|credential| {
                        credential.with_plain(|units| Secret::new(units.to_vec()))
                    }) {
                        Some(Ok(text)) => text,
                        Some(Err(e)) => {
                            warn!("the lent password could not be decrypted and is dropped: {e}");
                            state.credential = None;
                            let _ = reply.send(false);
                            return;
                        }
                        None => {
                            let _ = reply.send(false);
                            return;
                        }
                    };
                let sent = state.type_text(text);
                info!(sent, "lent password typed");
                let _ = reply.send(sent);
                return;
            }
            DesktopCommand::TypeText { text, reply } => {
                let sent = state.type_text(text);
                info!(sent, "text from a vault typed");
                let _ = reply.send(sent);
                return;
            }
            DesktopCommand::CredentialForget { reply } => {
                let held = state.credential.take().is_some();
                if held {
                    info!("lent password forgotten");
                }
                let _ = reply.send(held);
                let Some(prompt) = state.credential_prompt.take() else {
                    return;
                };
                let request_id = prompt.request_id;
                prompt.answer(CredentialEvent::Declined);
                IpcMessage::CredentialCancel { request_id }
            }
        };
        if let Some(helper) = &state.helper {
            let _ = helper.send(message);
        }
    }

    /// The session helper connected: route commands to it and replay the
    /// current state.
    pub fn attach(&self, helper: mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        let _ = helper.send(IpcMessage::Technicians(state.technicians.clone()));
        if let Some(branding) = &state.branding {
            let _ = helper.send(IpcMessage::Branding(branding.clone()));
        }
        state.helper = Some(helper);
        state.attached(HelperRole::User);
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
        state.assign();
        // Their prompts went with the helper.
        for (request_id, reply) in state.prompts.drain() {
            info!(request_id, "helper gone while a consent prompt was open");
            let _ = reply.send(PromptAnswer::Unavailable);
        }
        if let Some(prompt) = state.credential_prompt.take() {
            info!("helper gone while a password prompt was open");
            prompt.answer(CredentialEvent::Unavailable);
        }
    }

    /// The system helper connected: input goes to it from now on, and it
    /// captures whenever the session helper cannot.
    pub fn attach_system(&self, system: mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        state.system = Some(system);
        // Until it says otherwise.
        state.secure = false;
        state.attached(HelperRole::System);
    }

    /// `system` disconnected: input falls back to the session helper.
    pub fn detach_system(&self, system: &mpsc::UnboundedSender<IpcMessage>) {
        let mut state = self.state();
        if state
            .system
            .as_ref()
            .is_some_and(|s| s.same_channel(system))
        {
            state.system = None;
            state.secure = false;
            state.assign();
        }
    }

    /// Something the helper in role `from` sent.
    pub async fn from_helper(&self, from: HelperRole, message: IpcMessage) {
        let capturing = self.state().capturer == Some(from);
        match message {
            IpcMessage::Desktop { secure } if from == HelperRole::System => {
                let mut state = self.state();
                if state.secure != secure {
                    info!(secure, "the desktop on screen changed");
                    state.secure = secure;
                    state.assign();
                }
            }
            // From the helper that was capturing until a moment ago.
            IpcMessage::Monitors(_) | IpcMessage::Frame(_) if !capturing => {}
            // Only the user's own helper speaks for the user.
            IpcMessage::ConsentAnswer { .. }
            | IpcMessage::CredentialAnswer { .. }
            | IpcMessage::Clipboard(_)
            | IpcMessage::KillSwitch
                if from != HelperRole::User =>
            {
                warn!(?from, "ignoring a message only the session helper may send");
            }
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
                            if let Some(helper) = state.capturing() {
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
            IpcMessage::CredentialAnswer { request_id, secret } => {
                let mut state = self.state();
                if state
                    .credential_prompt
                    .as_ref()
                    .is_none_or(|p| p.request_id != request_id)
                {
                    debug!(request_id, "answer for a withdrawn password prompt");
                    return;
                }
                let event = match secret.as_ref().map(ProtectedSecret::new) {
                    Some(Ok(protected)) => {
                        state.credential = Some(protected);
                        CredentialEvent::Stored
                    }
                    Some(Err(e)) => {
                        warn!("the lent password could not be encrypted, so is not kept: {e}");
                        CredentialEvent::Unavailable
                    }
                    None => CredentialEvent::Declined,
                };
                info!(request_id, %event, "password prompt answered");
                if let Some(prompt) = state.credential_prompt.take() {
                    prompt.answer(event);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::desktop_channel;
    use crate::media::source::{media_channel, MediaLink};
    use protocol::ipc::EncodedFrame;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::{timeout, Duration};

    type Helper = UnboundedReceiver<IpcMessage>;

    fn bridge() -> (Arc<Bridge>, MediaLink) {
        let (media, media_source) = media_channel();
        let (_desktop, desktop_end) = desktop_channel(|| true);
        (Bridge::start(media_source, desktop_end), media)
    }

    fn frame() -> IpcMessage {
        IpcMessage::Frame(EncodedFrame {
            monitor: 0,
            keyframe: true,
            pts_us: 0,
            width: 16,
            height: 16,
            h264: vec![1],
        })
    }

    async fn next(helper: &mut Helper) -> IpcMessage {
        timeout(Duration::from_secs(5), helper.recv())
            .await
            .expect("a message within 5s")
            .expect("bridge alive")
    }

    /// Whether a frame from `from` reaches the network side.
    async fn forwarded(bridge: &Bridge, media: &MediaLink, from: HelperRole) -> bool {
        bridge.from_helper(from, frame()).await;
        let mut events = media.events.lock().await;
        matches!(events.try_recv(), Ok(MediaEvent::Frame(_)))
    }

    #[tokio::test]
    async fn capture_moves_to_the_system_helper_on_the_secure_desktop_and_back() {
        let (bridge, media) = bridge();
        let (user_tx, mut user) = mpsc::unbounded_channel();
        let (system_tx, mut system) = mpsc::unbounded_channel();
        bridge.attach(user_tx);
        bridge.attach_system(system_tx);
        assert_eq!(next(&mut user).await, IpcMessage::Technicians(vec![]));
        assert_eq!(next(&mut user).await, IpcMessage::ListMonitors);

        media
            .commands
            .send(MediaCommand::Start { monitor: 1 })
            .unwrap();
        assert_eq!(
            next(&mut user).await,
            IpcMessage::StartCapture { monitor: 1 }
        );
        assert!(forwarded(&bridge, &media, HelperRole::User).await);
        assert!(!forwarded(&bridge, &media, HelperRole::System).await);

        // The user locks the screen.
        bridge
            .from_helper(HelperRole::System, IpcMessage::Desktop { secure: true })
            .await;
        assert_eq!(next(&mut user).await, IpcMessage::StopCapture);
        assert_eq!(next(&mut system).await, IpcMessage::ListMonitors);
        assert_eq!(
            next(&mut system).await,
            IpcMessage::StartCapture { monitor: 1 }
        );
        assert!(forwarded(&bridge, &media, HelperRole::System).await);
        assert!(!forwarded(&bridge, &media, HelperRole::User).await);

        // And unlocks it.
        bridge
            .from_helper(HelperRole::System, IpcMessage::Desktop { secure: false })
            .await;
        assert_eq!(next(&mut system).await, IpcMessage::StopCapture);
        assert_eq!(next(&mut user).await, IpcMessage::ListMonitors);
        assert_eq!(
            next(&mut user).await,
            IpcMessage::StartCapture { monitor: 1 }
        );
        assert!(forwarded(&bridge, &media, HelperRole::User).await);
    }

    #[tokio::test]
    async fn the_system_helper_captures_alone_at_the_logon_screen() {
        let (bridge, media) = bridge();
        media
            .commands
            .send(MediaCommand::Start { monitor: 0 })
            .unwrap();
        // Let the bridge take the command before anything attaches.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (system_tx, mut system) = mpsc::unbounded_channel();
        bridge.attach_system(system_tx);
        assert_eq!(next(&mut system).await, IpcMessage::ListMonitors);
        assert_eq!(
            next(&mut system).await,
            IpcMessage::StartCapture { monitor: 0 }
        );
        assert!(forwarded(&bridge, &media, HelperRole::System).await);

        // Someone logs on: their helper takes over once their desktop shows.
        bridge
            .from_helper(HelperRole::System, IpcMessage::Desktop { secure: true })
            .await;
        let (user_tx, mut user) = mpsc::unbounded_channel();
        bridge.attach(user_tx);
        assert_eq!(next(&mut user).await, IpcMessage::Technicians(vec![]));
        assert!(forwarded(&bridge, &media, HelperRole::System).await);
        bridge
            .from_helper(HelperRole::System, IpcMessage::Desktop { secure: false })
            .await;
        assert_eq!(next(&mut system).await, IpcMessage::StopCapture);
        assert_eq!(next(&mut user).await, IpcMessage::ListMonitors);
        assert_eq!(
            next(&mut user).await,
            IpcMessage::StartCapture { monitor: 0 }
        );
    }

    #[tokio::test]
    async fn only_the_session_helper_speaks_for_the_user() {
        let (media, media_source) = media_channel();
        let (desktop, desktop_end) = desktop_channel(|| true);
        let bridge = Bridge::start(media_source, desktop_end);
        let _media = media;
        bridge
            .from_helper(HelperRole::System, IpcMessage::KillSwitch)
            .await;
        bridge
            .from_helper(HelperRole::User, IpcMessage::KillSwitch)
            .await;
        let mut events = desktop.events.lock().await;
        assert_eq!(events.try_recv(), Ok(DesktopEvent::KillSwitch));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_lent_password_is_kept_here_and_typed_by_the_system_helper() {
        let (media, media_source) = media_channel();
        let (desktop, desktop_end) = desktop_channel(|| true);
        let bridge = Bridge::start(media_source, desktop_end);
        let _media = media;
        let (user_tx, mut user) = mpsc::unbounded_channel();
        let (system_tx, mut system) = mpsc::unbounded_channel();
        bridge.attach(user_tx);
        bridge.attach_system(system_tx);
        let password: Vec<u16> = "hunter2".encode_utf16().collect();
        let type_it = || {
            let (reply, typed) = oneshot::channel();
            desktop
                .commands
                .send(DesktopCommand::CredentialType { reply })
                .unwrap();
            typed
        };

        // Nothing to type yet.
        assert!(!type_it().await.unwrap());

        let (reply, answer) = oneshot::channel();
        desktop
            .commands
            .send(DesktopCommand::CredentialPrompt {
                technician: "jane".into(),
                reply,
            })
            .unwrap();
        let request_id = loop {
            if let IpcMessage::CredentialPrompt {
                request_id,
                technician,
            } = next(&mut user).await
            {
                assert_eq!(technician, "jane");
                break request_id;
            }
        };
        // Only the user's own helper answers for the user.
        bridge
            .from_helper(
                HelperRole::System,
                IpcMessage::CredentialAnswer {
                    request_id,
                    secret: Some(Secret::new(vec![0x78])),
                },
            )
            .await;
        bridge
            .from_helper(
                HelperRole::User,
                IpcMessage::CredentialAnswer {
                    request_id,
                    secret: Some(Secret::new(password.clone())),
                },
            )
            .await;
        assert_eq!(answer.await.unwrap(), CredentialEvent::Stored);

        // Typed by the system helper, which reaches the lock screen.
        assert!(type_it().await.unwrap());
        let typed = loop {
            if let IpcMessage::TypeText(text) = next(&mut system).await {
                break text;
            }
        };
        assert_eq!(typed.units(), password);

        // Forgotten: nothing left to type, and an open prompt is withdrawn.
        let (reply, answer) = oneshot::channel();
        desktop
            .commands
            .send(DesktopCommand::CredentialPrompt {
                technician: "jane".into(),
                reply,
            })
            .unwrap();
        let (reply, forgotten) = oneshot::channel();
        desktop
            .commands
            .send(DesktopCommand::CredentialForget { reply })
            .unwrap();
        assert!(forgotten.await.unwrap());
        assert_eq!(answer.await.unwrap(), CredentialEvent::Declined);
        assert!(!type_it().await.unwrap());
        let withdrawn = loop {
            if let IpcMessage::CredentialCancel { request_id } = next(&mut user).await {
                break request_id;
            }
        };
        assert_eq!(withdrawn, request_id + 1);
    }
}
