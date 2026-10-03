//! The session helper: runs as the logged-on user, on their desktop.
//! Connects back to the service over the named pipe and exits when the pipe
//! closes (the service restarts it if it should run).
//!
//! It owns everything that must happen on the user's desktop:
//! - the tray icon: connection state, and everyone connected right now
//!   (tooltip and menu), with "End all remote sessions";
//! - the Ctrl+F12 hotkey, which ends every remote session;
//! - the on-screen session indicator naming who is connected;
//! - consent prompts (`require` mode) and "technician connected" toasts;
//! - the prompt for a password to lend the technicians;
//! - input injection, clipboard sync, and (in `stream`) screen capture.
//!
//! The parts that do not depend on the pipe are shared with the quick
//! assist client, which has no service and does all of this in one process
//! (see [`super::local`]): [`DesktopCommands`] carries out what is asked of
//! the desktop, and [`DesktopUi`] is the kill switch, the clipboard and the
//! indicator on a UI thread.
//!
//! Threads: the UI thread runs the tray, the hotkey and the clipboard
//! listener (all need its message loop); the pipe thread talks to the
//! service and injects input; capture, prompts and toasts have their own.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use protocol::clipboard::{ClipboardData, ClipboardGuard};
use protocol::ipc::{AgentStatus, IpcMessage, PIPE_NAME};
use protocol::{read_frame, write_frame};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::HSTRING;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, VK_F12,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MessageBoxW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage,
    MB_ICONINFORMATION, MB_OK, MSG, PM_REMOVE, QS_ALLINPUT, WM_HOTKEY,
};

use super::clipboard::Listener;
use super::indicator::Indicator;
use super::input::Injector;
use super::stream::{self, Desktops, WorkerCommand};
use super::{consent, credential, toast};
use crate::core;
use crate::interactive::indicator_text;

/// How often the indicator is put back on top of other topmost windows.
const INDICATOR_REFRESH: Duration = Duration::from_secs(3);

/// Windows error code when all pipe instances are busy.
const ERROR_PIPE_BUSY: i32 = 231;

/// Id of the Ctrl+F12 hotkey registration.
const KILL_SWITCH_HOTKEY: i32 = 1;

/// For the UI thread, from the thread that talks to the service.
pub enum UiEvent {
    Status(AgentStatus),
    Technicians(Vec<String>),
    SetClipboard(ClipboardData),
    /// The service went away; exit.
    Disconnected,
}

pub fn run() -> anyhow::Result<()> {
    common::logging::init_file(&crate::paths::helper_log());
    // Physical pixels everywhere, matching DXGI's monitor coordinates, so
    // injected pointer positions land where the viewer clicked. Must happen
    // before any window is created.
    // SAFETY: process-wide setting, no pointers.
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        warn!("setting DPI awareness: {e}");
    }
    toast::register();
    let session_id = current_session_id();
    info!(pid = std::process::id(), ?session_id, "helper starting");

    let (ui_tx, ui_rx) = mpsc::channel();
    // Control messages to the service (answers, clipboard, kill switch).
    let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn({
        let ctl_tx = ctl_tx.clone();
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let result =
                runtime.block_on(pipe_client(&ui_tx, ctl_tx, ctl_rx, session_id.unwrap_or(0)));
            if let Err(e) = result {
                warn!("pipe: {e}");
            }
            let _ = ui_tx.send(UiEvent::Disconnected);
        }
    });

    let tray = Tray::new(ctl_tx)?;
    tray.run(&ui_rx);
    info!("helper exiting");
    Ok(())
}

pub(super) fn current_session_id() -> Option<u32> {
    let mut session = 0;
    // SAFETY: out-parameter.
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session).ok()? };
    Some(session)
}

/// Frames and monitor lists queued for the service. Small: if the service
/// is not keeping up, the capture worker drops frames instead.
const OUT_QUEUE: usize = 8;

async fn pipe_client(
    ui: &mpsc::Sender<UiEvent>,
    ctl_tx: UnboundedSender<IpcMessage>,
    mut ctl_rx: UnboundedReceiver<IpcMessage>,
    session_id: u32,
) -> std::io::Result<()> {
    let pipe = connect().await?;
    let (mut reader, mut writer) = tokio::io::split(pipe);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<IpcMessage>(OUT_QUEUE);
    let mut commands = DesktopCommands::new(ui.clone(), ctl_tx, out_tx);
    write_frame(
        &mut writer,
        &IpcMessage::HelperHello {
            pid: std::process::id(),
            session_id,
            version: core::version().to_owned(),
        },
    )
    .await
    .map_err(std::io::Error::other)?;
    info!("connected to service");

    let write = async {
        loop {
            // Control messages (a kill switch!) go ahead of video.
            let message = tokio::select! {
                biased;
                Some(m) = ctl_rx.recv() => m,
                Some(m) = out_rx.recv() => m,
                else => break,
            };
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
            if !commands.handle(message) {
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

/// Carries out what the service asks of the user's desktop: input, consent
/// prompts, toasts and screen capture. Tray and clipboard changes are
/// passed on to the UI thread.
pub struct DesktopCommands {
    injector: Injector,
    /// Open prompts by request id, so they can be withdrawn.
    prompts: HashMap<u64, consent::Prompt>,
    /// Open password prompts, likewise.
    credential_prompts: HashMap<u64, credential::Prompt>,
    worker: mpsc::Sender<WorkerCommand>,
    ui: mpsc::Sender<UiEvent>,
    /// Answers, to the service.
    ctl: UnboundedSender<IpcMessage>,
}

impl DesktopCommands {
    /// Starts the capture worker; its frames and monitor lists go to `out`.
    pub fn new(
        ui: mpsc::Sender<UiEvent>,
        ctl: UnboundedSender<IpcMessage>,
        out: tokio::sync::mpsc::Sender<IpcMessage>,
    ) -> Self {
        Self {
            injector: Injector::default(),
            prompts: HashMap::new(),
            credential_prompts: HashMap::new(),
            worker: stream::spawn(out, Desktops::Own),
            ui,
            ctl,
        }
    }

    /// Act on one message from the service. `false` once the UI thread or
    /// the capture worker has gone, and there is nothing left to serve.
    pub fn handle(&mut self, message: IpcMessage) -> bool {
        let command = match message {
            IpcMessage::Status(status) => {
                info!(tooltip = %status.tooltip(), "status from service");
                return self.ui.send(UiEvent::Status(status)).is_ok();
            }
            IpcMessage::Technicians(list) => {
                info!(technicians = ?list, "connected technicians");
                let _ = self.ui.send(UiEvent::Technicians(list));
                return true;
            }
            IpcMessage::Input(event) => {
                if !self.injector.inject(event) {
                    warn!(?event, "input was blocked (secure desktop?)");
                }
                return true;
            }
            IpcMessage::SetClipboard(data) => {
                let _ = self.ui.send(UiEvent::SetClipboard(data));
                return true;
            }
            IpcMessage::Toast { technician } => {
                toast::technician_connected(&technician);
                return true;
            }
            IpcMessage::ConsentPrompt {
                request_id,
                technician,
                timeout_secs,
            } => {
                info!(request_id, %technician, timeout_secs, "showing consent prompt");
                let ctl = self.ctl.clone();
                let prompt = consent::show(
                    &technician,
                    Duration::from_secs(timeout_secs.into()),
                    move |answer| {
                        info!(request_id, ?answer, "consent answered");
                        let _ = ctl.send(IpcMessage::ConsentAnswer { request_id, answer });
                    },
                );
                self.prompts.insert(request_id, prompt);
                return true;
            }
            IpcMessage::ConsentCancel { request_id } => {
                if let Some(prompt) = self.prompts.remove(&request_id) {
                    info!(request_id, "consent prompt withdrawn");
                    prompt.cancel();
                }
                return true;
            }
            IpcMessage::CredentialPrompt {
                request_id,
                technician,
            } => {
                info!(request_id, %technician, "showing password prompt");
                let ctl = self.ctl.clone();
                let prompt = credential::show(&technician, move |secret| {
                    info!(
                        request_id,
                        typed = secret.is_some(),
                        "password prompt answered"
                    );
                    let _ = ctl.send(IpcMessage::CredentialAnswer { request_id, secret });
                });
                self.credential_prompts.insert(request_id, prompt);
                return true;
            }
            IpcMessage::CredentialCancel { request_id } => {
                if let Some(prompt) = self.credential_prompts.remove(&request_id) {
                    info!(request_id, "password prompt withdrawn");
                    prompt.cancel();
                }
                return true;
            }
            IpcMessage::TypeText(text) => {
                if !self.injector.type_text(text.units()) {
                    warn!("typing was blocked (secure desktop?)");
                }
                return true;
            }
            IpcMessage::ListMonitors => WorkerCommand::ListMonitors,
            IpcMessage::StartCapture { monitor } => WorkerCommand::Start { monitor },
            IpcMessage::StopCapture => WorkerCommand::Stop,
            IpcMessage::ForceKeyframe => WorkerCommand::ForceKeyframe,
            IpcMessage::SetBitrate { bps } => WorkerCommand::SetBitrate(bps),
            IpcMessage::SetFrameRate { fps } => WorkerCommand::SetFrameRate(fps),
            other => {
                debug!(?other, "ignoring message from service");
                return true;
            }
        };
        self.worker.send(command).is_ok()
    }
}

/// What must run on a UI thread (one with a message loop) while sessions
/// are possible: the Ctrl+F12 kill switch, clipboard sync and the
/// on-screen session indicator.
pub struct DesktopUi {
    clipboard: Option<Listener>,
    guard: RefCell<ClipboardGuard>,
    indicator: RefCell<Option<Indicator>>,
    indicator_refreshed: Cell<Instant>,
    /// To the service.
    ctl: UnboundedSender<IpcMessage>,
}

impl DesktopUi {
    /// Call on the UI thread; kill switch and clipboard messages go to `ctl`.
    pub fn new(ctl: UnboundedSender<IpcMessage>) -> Self {
        // The kill switch. Registered for this thread (no window), so
        // WM_HOTKEY arrives in its message loop.
        // SAFETY: no pointers.
        match unsafe {
            RegisterHotKey(
                None,
                KILL_SWITCH_HOTKEY,
                MOD_CONTROL | MOD_NOREPEAT,
                u32::from(VK_F12.0),
            )
        } {
            Ok(()) => info!("Ctrl+F12 kill switch registered"),
            // Another program holds Ctrl+F12; the tray item still works.
            Err(e) => warn!("registering Ctrl+F12: {e}"),
        }

        let clipboard = match Listener::new() {
            Ok(l) => Some(l),
            Err(e) => {
                warn!("clipboard sync disabled: {e}");
                None
            }
        };
        let indicator = match Indicator::new() {
            Ok(i) => Some(i),
            Err(e) => {
                warn!("session indicator unavailable: {e}");
                None
            }
        };
        let mut guard = ClipboardGuard::default();
        // Whatever the user copied before a technician connected stays private.
        guard.prime(clipboard.as_ref().and_then(Listener::read));
        Self {
            clipboard,
            guard: RefCell::new(guard),
            indicator: RefCell::new(indicator),
            indicator_refreshed: Cell::new(Instant::now()),
            ctl,
        }
    }

    /// Whether `msg`, from this thread's queue, is the Ctrl+F12 hotkey.
    pub fn is_kill_switch(&self, msg: &MSG) -> bool {
        msg.message == WM_HOTKEY && msg.wParam.0 == KILL_SWITCH_HOTKEY as usize
    }

    /// End every remote session now.
    pub fn kill_switch(&self, via: &str) {
        warn!(via, "user ended all remote sessions");
        let _ = self.ctl.send(IpcMessage::KillSwitch);
    }

    /// Call every turn of the message loop: passes on a changed clipboard,
    /// and keeps the indicator on top.
    pub fn poll(&self) {
        if let Some(clipboard) = self.clipboard.as_ref().filter(|c| c.changed()) {
            if let Some(data) = clipboard.read() {
                if let Some(data) = self.guard.borrow_mut().local_changed(data) {
                    debug!(bytes = data.size(), "clipboard changed");
                    let _ = self.ctl.send(IpcMessage::Clipboard(data));
                }
            }
        }
        if self.indicator_refreshed.get().elapsed() >= INDICATOR_REFRESH {
            self.indicator_refreshed.set(Instant::now());
            if let Some(indicator) = self.indicator.borrow_mut().as_mut() {
                indicator.refresh();
            }
        }
    }

    /// Who is connected now: shows, updates or hides the indicator.
    pub fn set_technicians(&self, list: &[String]) {
        if let Some(indicator) = self.indicator.borrow_mut().as_mut() {
            indicator.set(indicator_text(list));
        }
    }

    /// Put the technician's clipboard on the user's.
    pub fn set_clipboard(&self, data: &ClipboardData) {
        let Some(clipboard) = &self.clipboard else {
            return;
        };
        if !self.guard.borrow_mut().remote(data) {
            return;
        }
        match clipboard.write(data) {
            Ok(()) => {
                info!(bytes = data.size(), "technician's clipboard applied");
                // What the listener reads back next is not a new copy.
                self.guard
                    .borrow_mut()
                    .prime(Some(ClipboardData::Text(data.to_text())));
            }
            Err(e) => warn!("setting clipboard: {e}"),
        }
    }
}

impl Drop for DesktopUi {
    fn drop(&mut self) {
        // SAFETY: undoes our registration on this thread.
        let _ = unsafe { UnregisterHotKey(None, KILL_SWITCH_HOTKEY) };
    }
}

pub(super) async fn connect() -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
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
    technicians_item: MenuItem,
    end_sessions_item: MenuItem,
    about_item: MenuItem,
    status: RefCell<AgentStatus>,
    technicians: RefCell<Vec<String>>,
    /// The tooltip/icon still need applying.
    ///
    /// At logon the helper can start before Explorer has created the taskbar.
    /// Then `Shell_NotifyIcon(NIM_MODIFY)` fails, and tray-icon 0.25 returns
    /// the error *without* recording the new tooltip/icon. When the taskbar
    /// appears it re-adds the icon with the stale, initial values. So a
    /// change stays pending, and is retried every loop iteration, until the
    /// modify succeeds. After that tray-icon has the current values, which
    /// also covers Explorer restarts.
    dirty: Cell<bool>,
    desktop: DesktopUi,
}

impl Tray {
    fn new(ctl: UnboundedSender<IpcMessage>) -> anyhow::Result<Self> {
        let initial = core::initial_status(None);
        let status_item = MenuItem::new(initial.tooltip(), false, None);
        let technicians_item = MenuItem::new(technicians_text(&[]), false, None);
        let end_sessions_item = MenuItem::new("End all remote sessions (Ctrl+F12)", false, None);
        let about_item = MenuItem::new("About RMM Agent", true, None);
        // Deliberately disabled: users cannot stop the agent from the tray.
        let quit_item = MenuItem::new("Quit (managed by your administrator)", false, None);
        let menu = Menu::new();
        menu.append(&status_item)?;
        menu.append(&technicians_item)?;
        menu.append(&end_sessions_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&about_item)?;
        menu.append(&quit_item)?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(initial.tooltip())
            .with_icon(status_icon(&initial, false))
            .build()?;

        Ok(Self {
            icon,
            status_item,
            technicians_item,
            end_sessions_item,
            about_item,
            status: RefCell::new(initial),
            technicians: RefCell::new(Vec::new()),
            dirty: Cell::new(false),
            desktop: DesktopUi::new(ctl),
        })
    }

    /// Win32 message loop plus polling of menu, clipboard and service
    /// events. Returns when the service disconnects.
    fn run(&self, ui: &mpsc::Receiver<UiEvent>) {
        loop {
            // Sleep until there is window input, or at most 100 ms.
            // SAFETY: no handles; just waits on this thread's message queue.
            unsafe {
                MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
            }
            let mut msg = MSG::default();
            // SAFETY: standard message pump on the thread that owns the
            // tray and clipboard windows.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if self.desktop.is_kill_switch(&msg) {
                        self.desktop.kill_switch("hotkey");
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                debug!(id = ?event.id, "menu event");
                if event.id == *self.about_item.id() {
                    show_about();
                } else if event.id == *self.end_sessions_item.id() {
                    self.desktop.kill_switch("tray menu");
                }
            }
            // Clicks on the icon itself are not used; drain them.
            while TrayIconEvent::receiver().try_recv().is_ok() {}

            loop {
                match ui.try_recv() {
                    Ok(UiEvent::Status(status)) => {
                        info!(tooltip = %status.tooltip(), "status changed");
                        *self.status.borrow_mut() = status;
                        self.dirty.set(true);
                    }
                    Ok(UiEvent::Technicians(list)) => {
                        self.desktop.set_technicians(&list);
                        *self.technicians.borrow_mut() = list;
                        self.dirty.set(true);
                    }
                    Ok(UiEvent::SetClipboard(data)) => self.desktop.set_clipboard(&data),
                    Ok(UiEvent::Disconnected) | Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            self.apply();
            self.desktop.poll();
        }
    }

    fn apply(&self) {
        if !self.dirty.get() {
            return;
        }
        let status = self.status.borrow();
        let technicians = self.technicians.borrow();
        let tooltip = status.tooltip_with(&technicians);
        self.status_item.set_text(status.tooltip());
        self.technicians_item
            .set_text(technicians_text(&technicians));
        self.end_sessions_item.set_enabled(!technicians.is_empty());
        let applied = self.icon.set_tooltip(Some(&tooltip)).is_ok()
            && self
                .icon
                .set_icon(Some(status_icon(&status, !technicians.is_empty())))
                .is_ok();
        if applied {
            info!(%tooltip, "tray updated");
            self.dirty.set(false);
        }
    }
}

fn technicians_text(technicians: &[String]) -> String {
    if technicians.is_empty() {
        "No remote sessions".to_owned()
    } else {
        format!("Connected: {}", technicians.join(", "))
    }
}

fn show_about() {
    let text = format!(
        "RMM Agent {}\n\nThis computer is managed remotely by your IT team.\n\
         Press Ctrl+F12 at any time to end all remote sessions.",
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
    debug!(?result, "about box closed");
}

/// A 32x32 filled circle: blue while a technician is connected, green when
/// connected to the server, amber when enrolled but disconnected, grey when
/// not enrolled.
fn status_icon(status: &AgentStatus, in_session: bool) -> Icon {
    let rgb = match (&status.agent_id, status.connected) {
        _ if in_session => [0x1f, 0x6f, 0xeb],
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
