//! The session helper: runs as the logged-on user, on their desktop, and
//! shows the tray icon. Connects back to the service over the named pipe and
//! exits when the pipe closes (the service restarts it if it should run).
//!
//! In this phase it is a shell: icon, tooltip showing connection state, and
//! a menu with About and a disabled Quit (the user cannot stop the agent).

use std::sync::mpsc;
use std::time::Duration;

use protocol::ipc::{AgentStatus, IpcMessage, PIPE_NAME};
use protocol::{read_frame, write_frame};
use tokio::net::windows::named_pipe::ClientOptions;
use tracing::{info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::HSTRING;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MessageBoxW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage,
    MB_ICONINFORMATION, MB_OK, MSG, PM_REMOVE, QS_ALLINPUT,
};

use super::stream::{self, WorkerCommand};
use crate::core;

/// Windows error code when all pipe instances are busy.
const ERROR_PIPE_BUSY: i32 = 231;

enum UiEvent {
    Status(AgentStatus),
    /// The service went away; exit.
    Disconnected,
}

pub fn run() -> anyhow::Result<()> {
    common::logging::init_file(&crate::paths::helper_log());
    let session_id = current_session_id();
    info!(pid = std::process::id(), ?session_id, "helper starting");

    let (ui_tx, ui_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        if let Err(e) = runtime.block_on(pipe_client(&ui_tx, session_id.unwrap_or(0))) {
            warn!("pipe: {e}");
        }
        let _ = ui_tx.send(UiEvent::Disconnected);
    });

    let tray = Tray::new()?;
    tray.run(&ui_rx);
    info!("helper exiting");
    Ok(())
}

fn current_session_id() -> Option<u32> {
    let mut session = 0;
    // SAFETY: out-parameter.
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session).ok()? };
    Some(session)
}

/// Frames and monitor lists queued for the service. Small: if the service
/// is not keeping up, the capture worker drops frames instead.
const OUT_QUEUE: usize = 8;

async fn pipe_client(ui: &mpsc::Sender<UiEvent>, session_id: u32) -> std::io::Result<()> {
    let pipe = connect().await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<IpcMessage>(OUT_QUEUE);
    // Hello first; the channel keeps order.
    let _ = out_tx
        .send(IpcMessage::HelperHello {
            pid: std::process::id(),
            session_id,
            version: core::version().to_owned(),
        })
        .await;
    let worker = stream::spawn(out_tx);
    info!("connected to service");

    let write = async {
        while let Some(message) = out_rx.recv().await {
            write_frame(&mut writer, &message)
                .await
                .map_err(std::io::Error::other)?;
        }
        Ok::<(), std::io::Error>(())
    };
    let read = async {
        while let Some(message) = read_frame::<_, IpcMessage>(&mut reader)
            .await
            .map_err(std::io::Error::other)?
        {
            let command = match message {
                IpcMessage::Status(status) => {
                    info!(tooltip = %status.tooltip(), "status from service");
                    if ui.send(UiEvent::Status(status)).is_err() {
                        break;
                    }
                    continue;
                }
                IpcMessage::ListMonitors => WorkerCommand::ListMonitors,
                IpcMessage::StartCapture { monitor } => WorkerCommand::Start { monitor },
                IpcMessage::StopCapture => WorkerCommand::Stop,
                IpcMessage::ForceKeyframe => WorkerCommand::ForceKeyframe,
                _ => continue,
            };
            if worker.send(command).is_err() {
                break;
            }
        }
        Ok(())
    };
    tokio::select! {
        r = write => r,
        r = read => r,
    }
}

async fn connect() -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(pipe) => return Ok(pipe),
            // Busy (between instances) or not created yet: the service is
            // starting or just accepted another connection. Retry briefly.
            Err(e)
                if attempts < 40
                    && (e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        || e.kind() == std::io::ErrorKind::NotFound) =>
            {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

struct Tray {
    icon: TrayIcon,
    status_item: MenuItem,
    about_item: MenuItem,
    /// A status not yet successfully applied to the tray icon.
    ///
    /// At logon the helper can start before Explorer has created the taskbar.
    /// Then `Shell_NotifyIcon(NIM_MODIFY)` fails, and tray-icon 0.25 returns
    /// the error *without* recording the new tooltip/icon. When the taskbar
    /// appears it re-adds the icon with the stale, initial values. So a
    /// status stays pending, and is retried every loop iteration, until the
    /// modify succeeds. After that tray-icon has the current values, which
    /// also covers Explorer restarts.
    pending: std::cell::RefCell<Option<AgentStatus>>,
}

impl Tray {
    fn new() -> anyhow::Result<Self> {
        let initial = core::initial_status(None);
        let status_item = MenuItem::new(initial.tooltip(), false, None);
        let about_item = MenuItem::new("About RMM Agent", true, None);
        // Deliberately disabled: users cannot stop the agent from the tray.
        let quit_item = MenuItem::new("Quit (managed by your administrator)", false, None);
        let menu = Menu::new();
        menu.append(&status_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&about_item)?;
        menu.append(&quit_item)?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(initial.tooltip())
            .with_icon(status_icon(&initial))
            .build()?;
        Ok(Self {
            icon,
            status_item,
            about_item,
            pending: std::cell::RefCell::new(None),
        })
    }

    /// Win32 message loop plus polling of menu and status events. Returns
    /// when the service disconnects.
    fn run(&self, ui: &mpsc::Receiver<UiEvent>) {
        loop {
            // Sleep until there is window input, or at most 100 ms.
            // SAFETY: no handles; just waits on this thread's message queue.
            unsafe {
                MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
            }
            let mut msg = MSG::default();
            // SAFETY: standard message pump on the thread that owns the tray window.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                tracing::debug!(id = ?event.id, "menu event");
                if event.id == *self.about_item.id() {
                    show_about();
                }
            }
            // Clicks on the icon itself are not used yet; drain them.
            while TrayIconEvent::receiver().try_recv().is_ok() {}

            loop {
                match ui.try_recv() {
                    Ok(UiEvent::Status(status)) => {
                        info!(tooltip = %status.tooltip(), "status changed");
                        *self.pending.borrow_mut() = Some(status);
                    }
                    Ok(UiEvent::Disconnected) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            self.apply_pending();
        }
    }

    fn apply_pending(&self) {
        let Some(status) = self.pending.borrow().clone() else {
            return;
        };
        let text = status.tooltip();
        self.status_item.set_text(&text);
        let applied = self.icon.set_tooltip(Some(&text)).is_ok()
            && self.icon.set_icon(Some(status_icon(&status))).is_ok();
        if applied {
            info!(%text, "tray updated");
            *self.pending.borrow_mut() = None;
        }
    }
}

fn show_about() {
    let text = format!(
        "RMM Agent {}\n\nThis computer is managed remotely by your IT team.",
        core::version()
    );
    // SAFETY: plain modal message box with owned strings.
    let result = unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from("About RMM Agent"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
    tracing::debug!(?result, "about box closed");
}

/// A 32x32 filled circle: green when connected, amber when enrolled but
/// disconnected, grey when not enrolled.
fn status_icon(status: &AgentStatus) -> Icon {
    let rgb = match (&status.agent_id, status.connected) {
        (Some(_), true) => [0x2e, 0xa0, 0x43],
        (Some(_), false) => [0xd9, 0x8e, 0x04],
        (None, _) => [0x8a, 0x8a, 0x8a],
    };
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
            // Anti-aliased edge.
            let alpha = (15.5 - d).clamp(0.0, 1.0);
            rgba.extend_from_slice(&rgb);
            rgba.push((alpha * 255.0) as u8);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon dimensions")
}
