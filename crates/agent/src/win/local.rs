//! The user's desktop without a service: for the quick assist client, which
//! the user runs themselves, in their own session, for one session.
//!
//! An installed agent splits the work between the service and its helpers
//! (see [`super`]), joined by a named pipe. Here the same pieces run in one
//! process: the agent core talks to the [`Bridge`] as the service's does,
//! and the bridge is attached to [`DesktopCommands`] over channels where
//! the pipe would be (compare `pipe::serve_helper` and
//! `helper::pipe_client`).
//!
//! Input is injected by this process, at whatever integrity level the user
//! started it with: if they let it elevate, it reaches elevated windows;
//! the secure desktop (UAC prompts, the lock screen) stays out of reach
//! either way.

use std::sync::{mpsc, Arc};

use protocol::ipc::IpcMessage;
use tokio::sync::mpsc::{channel, unbounded_channel, UnboundedSender};
use tracing::info;

use super::bridge::Bridge;
use super::helper::{DesktopCommands, UiEvent};
use crate::interactive::{desktop_channel, DesktopLink};
use crate::media::source::{media_channel, MediaLink};

/// Frames and monitor lists queued for the bridge (as the helper's queue
/// for the pipe: if the network side is behind, the worker drops frames).
const OUT_QUEUE: usize = 8;

/// The desktop's ends, for the agent core and for the UI thread.
pub struct LocalDesktop {
    /// Screen source for `CoreOptions::media`.
    pub media: Arc<MediaLink>,
    /// The desktop for `CoreOptions::desktop`.
    pub desktop: Arc<DesktopLink>,
    /// What the UI thread must act on (technicians, clipboard).
    pub ui: mpsc::Receiver<UiEvent>,
    /// For the UI thread's [`super::helper::DesktopUi`]: the kill switch
    /// and clipboard changes.
    pub ctl: UnboundedSender<IpcMessage>,
}

/// Start capture, input, prompts and the bridge. Call inside a tokio
/// runtime; it all stops when the runtime does.
pub fn start() -> LocalDesktop {
    let (media, media_source) = media_channel();
    // Whoever runs this is the user, and they are here.
    let (desktop, desktop_end) = desktop_channel(|| true);
    let bridge = Bridge::start(media_source, desktop_end);

    let (ui_tx, ui) = mpsc::channel();
    let (ctl, mut ctl_rx) = unbounded_channel::<IpcMessage>();
    let (out_tx, mut out_rx) = channel::<IpcMessage>(OUT_QUEUE);
    let (to_desktop, mut commands_rx) = unbounded_channel::<IpcMessage>();
    let mut commands = DesktopCommands::new(ui_tx, ctl.clone(), out_tx);
    bridge.attach(to_desktop);

    // Bridge to desktop. Consent prompts and capture have threads of their
    // own, so nothing here blocks.
    tokio::spawn(async move {
        while let Some(message) = commands_rx.recv().await {
            if !commands.handle(message) {
                break;
            }
        }
        info!("local desktop stopped");
    });
    // Desktop to bridge. Control messages (a kill switch!) go ahead of video.
    tokio::spawn(async move {
        loop {
            let message = tokio::select! {
                biased;
                Some(m) = ctl_rx.recv() => m,
                Some(m) = out_rx.recv() => m,
                else => break,
            };
            bridge.from_helper(message).await;
        }
    });

    LocalDesktop {
        media: Arc::new(media),
        desktop: Arc::new(desktop),
        ui,
        ctl,
    }
}
