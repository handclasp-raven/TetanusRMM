//! The session helper: runs as the logged-on user, on their desktop.
//! Connects back to the service over the named pipe and exits when the pipe
//! closes (the service restarts it if it should run).
//!
//! It owns everything that must happen on the user's desktop:
//! - the tray icon: connection state, and everyone connected right now
//!   (tooltip and flyout), with "End all remote sessions";
//! - the Ctrl+F12 hotkey, which ends every remote session;
//! - the on-screen session bar naming who is connected, with its own
//!   "End session";
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
use std::time::Duration;

use brand::mark::TrayState;
use brand::raster;
use protocol::clipboard::{ClipboardData, ClipboardGuard};
use protocol::ipc::{AgentStatus, IpcMessage, PIPE_NAME};
use protocol::{read_frame, write_frame};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};
use tray_icon::{Icon, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::RECT;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, VK_F12,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetSystemMetrics, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage,
    MSG, PM_REMOVE, QS_ALLINPUT, SM_CXSMICON, WM_HOTKEY,
};

use super::clipboard::Listener;
use super::flyout::{self, Flyout};
use super::indicator::Indicator;
use super::input::Injector;
use super::stream::{self, Desktops, WorkerCommand};
use super::ui::about;
use super::ui::look::{self, Look};
use super::{consent, credential, toast};
use crate::core;
use crate::interactive::indicator_who;

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
    // The branding last heard of, until the service says what it is now.
    look::load_cached();
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
    /// Whether the branding is kept for the next start (the helper), or
    /// only used (quick assist, which leaves nothing behind).
    remember_branding: bool,
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
            remember_branding: true,
        }
    }

    /// Use the branding the server sends without keeping it on disk.
    pub fn without_brand_cache(mut self) -> Self {
        self.remember_branding = false;
        self
    }

    /// Act on one message from the service. `false` once the UI thread or
    /// the capture worker has gone, and there is nothing left to serve.
    pub fn handle(&mut self, message: IpcMessage) -> bool {
        let command = match message {
            IpcMessage::Status(status) => {
                info!(state = status.state(), "status from service");
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
            IpcMessage::Branding(branding) => {
                // Every window follows by itself (see `look::stamp`).
                if self.remember_branding {
                    look::set_branding(branding);
                } else {
                    look::use_branding(branding);
                }
                toast::register();
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
    /// looks after the session bar, and ends the sessions if its button
    /// was pressed.
    pub fn poll(&self) {
        if let Some(clipboard) = self.clipboard.as_ref().filter(|c| c.changed()) {
            if let Some(data) = clipboard.read() {
                if let Some(data) = self.guard.borrow_mut().local_changed(data) {
                    debug!(bytes = data.size(), "clipboard changed");
                    let _ = self.ctl.send(IpcMessage::Clipboard(data));
                }
            }
        }
        let ended = self.indicator.borrow_mut().as_mut().is_some_and(|bar| {
            bar.tick();
            bar.take_end()
        });
        if ended {
            self.kill_switch("session bar");
        }
    }

    /// Who is connected now: shows, updates or hides the indicator.
    pub fn set_technicians(&self, list: &[String]) {
        if let Some(indicator) = self.indicator.borrow_mut().as_mut() {
            indicator.set(indicator_who(list));
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
    /// What a click on the icon opens (see `super::flyout`).
    flyout: Option<Flyout>,
    status: RefCell<AgentStatus>,
    technicians: RefCell<Vec<String>>,
    /// The look the icon was last drawn with (see [`look::stamp`]).
    stamp: Cell<(u64, bool, bool)>,
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
        // No menu: a click opens the flyout.
        let icon = TrayIconBuilder::new()
            .with_tooltip(initial.tooltip(&Look::current().agent_name()))
            .with_icon(tray_image(&initial, false))
            .build()?;
        let flyout = match Flyout::new(initial.clone()) {
            Ok(flyout) => Some(flyout),
            Err(e) => {
                warn!("the tray flyout is unavailable: {e}");
                None
            }
        };

        Ok(Self {
            icon,
            flyout,
            status: RefCell::new(initial),
            technicians: RefCell::new(Vec::new()),
            stamp: Cell::new(look::stamp()),
            dirty: Cell::new(false),
            desktop: DesktopUi::new(ctl),
        })
    }

    /// Win32 message loop plus polling of the tray, clipboard and service
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
            // A click on the icon, either button, opens the flyout.
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    rect,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    let (left, top) = (rect.position.x as i32, rect.position.y as i32);
                    if let Some(flyout) = &self.flyout {
                        flyout.toggle(RECT {
                            left,
                            top,
                            right: left + rect.size.width as i32,
                            bottom: top + rect.size.height as i32,
                        });
                    }
                }
            }
            if let Some(flyout) = &self.flyout {
                flyout.tick();
            }
            for action in self.flyout.iter().flat_map(Flyout::take_actions) {
                debug!(?action, "tray flyout");
                match action {
                    flyout::Action::About => about::show(),
                    flyout::Action::EndSessions => self.desktop.kill_switch("tray"),
                }
            }
            // Light or dark, or the branding, changed: the icon follows.
            let stamp = look::stamp();
            if self.stamp.replace(stamp) != stamp {
                self.dirty.set(true);
                if let Some(flyout) = &self.flyout {
                    flyout.restyle();
                }
            }

            loop {
                match ui.try_recv() {
                    Ok(UiEvent::Status(status)) => {
                        info!(state = status.state(), "status changed");
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
        let tooltip = status.tooltip_with(&Look::current().agent_name(), &technicians);
        if let Some(flyout) = &self.flyout {
            flyout.update(&status, &technicians);
        }
        let applied = self.icon.set_tooltip(Some(&tooltip)).is_ok()
            && self
                .icon
                .set_icon(Some(tray_image(&status, !technicians.is_empty())))
                .is_ok();
        if applied {
            info!(%tooltip, "tray updated");
            self.dirty.set(false);
        }
    }
}

/// The tray icon: the mark (or the company's logo) with a badge for the
/// state, green when connected to the server, red while a technician is
/// connected here; grey and without one when offline or not enrolled.
fn tray_image(status: &AgentStatus, in_session: bool) -> Icon {
    let state = match (&status.agent_id, status.connected) {
        _ if in_session => TrayState::Live,
        (Some(_), true) => TrayState::Connected,
        _ => TrayState::Offline,
    };
    // SAFETY: a plain metric query.
    let size = match unsafe { GetSystemMetrics(SM_CXSMICON) } {
        size if size > 0 => size as u32,
        _ => 16,
    };
    let look = Look::current();
    let image = match &look.logo {
        Some(logo) => {
            let mut image = raster::fit(&logo.rgba, logo.width, logo.height, size);
            raster::badge(&mut image, state);
            image
        }
        None => raster::tray_icon(
            size,
            state,
            raster::tray_tile(look::taskbar_dark(), look.accent),
        ),
    };
    Icon::from_rgba(image.rgba, size, size).expect("valid icon dimensions")
}
