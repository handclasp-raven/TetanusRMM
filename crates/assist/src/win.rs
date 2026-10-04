//! The quick assist window, and everything around it on Windows.
//!
//! Start-up: ask for elevation (so the technician's input can reach
//! elevated windows), carrying on without it if the user says no. Then one
//! window (drawn with the agent's dialog toolkit, `agent::win::ui`, in
//! the brand's look and the company's branding if the download carries
//! one) with three pages:
//!
//! 1. The scam warning. It cannot be accepted for
//!    [`WARNING_DELAY_SECS`] seconds: the button is disabled and counts
//!    down. Nothing touches the network, the screen or the clipboard until
//!    it has been accepted.
//! 2. The code, typed into six boxes.
//! 3. The session: once the server accepts the code, this process is the
//!    session's agent ([`crate::session`]) with the desktop in-process
//!    (`agent::win::local`), until the window is closed.
//!
//! The dialog only records what was pressed; the message loop in
//! [`App::run`] acts on it, alongside what the session and the desktop
//! report. That loop is also where the Ctrl+F12 kill switch, clipboard
//! sync and the on-screen session bar live, as in the agent's helper.

use std::sync::mpsc;
use std::time::Instant;

use agent::win::helper::{DesktopUi, UiEvent};
use agent::win::local::{self, LocalDesktop};
use agent::win::ui::dialog::{
    Block, Button, Dialog, Event as Pressed, Focus, Lead, Options, Row, Spec, Tone,
};
use agent::win::ui::look;
use brand::icons;
use protocol::assist::{
    accept_button, normalize_code, AssistConfig, CODE_LEN, WARNING_DELAY_SECS, WARNING_PAYMENT,
    WARNING_SUBTITLE, WARNING_TITLE, WARNING_UNEXPECTED,
};
use protocol::ipc::AgentStatus;
use tokio::sync::watch;
use tracing::{info, warn};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MessageBoxW, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage,
    MB_ICONERROR, MB_OK, MSG, PM_REMOVE, QS_ALLINPUT, SW_SHOWNORMAL,
};

use crate::session::{self, Links, Phase};

const TITLE: &str = "TetanusRMM Quick Assist";

/// Passed to the elevated copy of this program, so that it does not ask
/// for elevation again.
const NO_ELEVATE: &str = "--no-elevate";

// Buttons.
const ACCEPT: u32 = 100;
const CLOSE: u32 = 101;
const CONNECT: u32 = 102;
const END: u32 = 103;

/// How long the connection gets to tell the server it is closing.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// Whether this process runs elevated.
fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle for this process; the token is closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0;
    // SAFETY: the buffer is a TOKEN_ELEVATION of the size given.
    let known = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: we own the handle.
    unsafe {
        let _ = CloseHandle(token);
    }
    known.is_ok() && elevation.TokenIsElevated != 0
}

/// Start this program again, elevated (Windows asks the user). `false` if
/// they said no, or it could not be started.
fn relaunch_elevated() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // SAFETY: owned strings that outlive the call.
    let started = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &HSTRING::from(exe.as_os_str()),
            &HSTRING::from(NO_ELEVATE),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // By its documentation: a value above 32 means it started.
    started.0 as isize > 32
}

fn message_box(text: &str) {
    // SAFETY: plain modal message box with owned strings.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(TITLE),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// From the session's task to the window.
enum Event {
    /// The code was accepted; the session's connection reports here.
    Started(watch::Receiver<AgentStatus>),
    /// The code was refused, or the session could not run: say why, and
    /// let the user try again.
    Failed(String),
}

/// The scam warning, with `remaining` seconds before it can be accepted.
/// The keyboard starts on Close, the page's main button: backing out is
/// always the easy thing to do.
fn warning_page(remaining: u32) -> Spec {
    let (accept, enabled) = accept_button(remaining);
    let point = |icon, text: &str| Row {
        icon,
        tone: Tone::Live,
        text: text.to_owned(),
    };
    Spec {
        title: TITLE.into(),
        blocks: vec![
            Block::Header {
                lead: Lead::Icon(icons::SAFETY),
                title: WARNING_TITLE.into(),
                subtitle: WARNING_SUBTITLE.into(),
            },
            Block::Panel(vec![
                point(icons::PROHIBITED, WARNING_PAYMENT),
                point(icons::PHONE, WARNING_UNEXPECTED),
            ]),
        ],
        buttons: vec![
            Button::new(ACCEPT, accept).enabled(enabled),
            Button::new(CLOSE, "Close").primary(),
        ],
        focus: Focus::Button(CLOSE),
        default: None,
        // Escape does nothing: the window is closed on purpose.
        cancel: None,
    }
}

/// The code page: `entry` while a code can be typed and sent, and what
/// there is to say under the boxes.
fn code_page(entry: bool, status: &str, error: bool) -> Spec {
    Spec {
        title: TITLE.into(),
        blocks: vec![
            Block::Header {
                lead: Lead::Icon(icons::CODE),
                title: "Enter your support code".into(),
                subtitle: format!(
                    "Type the {CODE_LEN}-digit code the person supporting you gave you."
                ),
            },
            Block::Code {
                label: "Support code".into(),
                note: "Nobody can see your screen until you approve it.".into(),
                enabled: entry,
            },
            Block::Status {
                text: status.to_owned(),
                error,
            },
        ],
        buttons: vec![
            Button::new(END, "End session").left(),
            Button::new(CONNECT, "Connect").primary().enabled(entry),
        ],
        focus: Focus::Input,
        default: Some(CONNECT),
        cancel: None,
    }
}

/// The names in `technicians`, each once.
fn names(technicians: &[String]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for name in technicians {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    names.join(", ")
}

/// The session page: the code was accepted, and this is how it stands.
fn session_page(connected: bool, technicians: &[String]) -> Spec {
    let (title, subtitle, rows) = if !connected {
        (
            "Connecting\u{2026}".to_owned(),
            "Reaching the support server.".to_owned(),
            Vec::new(),
        )
    } else if technicians.is_empty() {
        (
            "Waiting for your supporter".to_owned(),
            "You will be asked before they can see your screen.".to_owned(),
            Vec::new(),
        )
    } else {
        let who = names(technicians);
        let verb = if who.contains(", ") { "are" } else { "is" };
        (
            format!("{who} {verb} connected"),
            "They can see and control this computer.".to_owned(),
            vec![
                Row {
                    icon: icons::END,
                    tone: Tone::Live,
                    text: "Press Ctrl+F12 to stop them at any time".into(),
                },
                Row {
                    icon: icons::CLOSE,
                    tone: Tone::Accent,
                    text: "Close this window to end the session".into(),
                },
            ],
        )
    };
    let mut blocks = vec![Block::Header {
        lead: Lead::Icon(icons::REMOTE),
        title,
        subtitle,
    }];
    if !rows.is_empty() {
        blocks.push(Block::Rows(rows));
    }
    Spec {
        title: TITLE.into(),
        blocks,
        buttons: vec![Button::new(END, "End session").primary()],
        focus: Focus::Button(END),
        default: None,
        cancel: None,
    }
}

/// The desktop, once the warning has been accepted.
struct Desktop {
    local: LocalDesktop,
    ui: DesktopUi,
}

struct App {
    dialog: Dialog,
    elevated: bool,
    config: AssistConfig,
    runtime: tokio::runtime::Runtime,
    /// When the warning was shown: it is accepted only once its whole
    /// delay has passed.
    warned: Instant,
    accepted: bool,
    desktop: Option<Desktop>,
    phase: Phase,
    /// Why the last code did not work.
    error: Option<String>,
    events: (mpsc::Sender<Event>, mpsc::Receiver<Event>),
    connection: Option<watch::Receiver<AgentStatus>>,
    /// Set to end the session's connection (on the way out).
    stop: watch::Sender<bool>,
    technicians: Vec<String>,
}

impl App {
    /// Seconds of the warning's delay left.
    fn remaining(&self) -> u32 {
        WARNING_DELAY_SECS.saturating_sub(self.warned.elapsed().as_secs() as u32)
    }

    /// What the window should show now.
    fn page(&self) -> Spec {
        if !self.accepted {
            return warning_page(self.remaining());
        }
        let connected = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.borrow().connected);
        match (self.phase, &self.error) {
            (Phase::Session, _) => session_page(connected, &self.technicians),
            (Phase::Checking, _) => code_page(false, "Checking the code\u{2026}", false),
            (Phase::Idle, Some(error)) => code_page(true, error, true),
            (Phase::Idle, None) if !self.elevated => code_page(
                true,
                "Running without administrator rights: your supporter cannot control \
                 administrator windows.",
                false,
            ),
            (Phase::Idle, None) => code_page(true, "", false),
        }
    }

    /// Act on a button. `false` to close.
    fn handle(&mut self, pressed: Pressed) -> bool {
        match pressed {
            Pressed::Close | Pressed::Button(CLOSE | END) => return false,
            // Only a press that comes after the whole delay counts.
            Pressed::Button(ACCEPT) if !self.accepted && self.remaining() == 0 => {
                info!("scam warning accepted");
                self.accepted = true;
                let _runtime = self.runtime.enter();
                let local = local::start();
                let ui = DesktopUi::new(local.ctl.clone());
                self.desktop = Some(Desktop { local, ui });
            }
            Pressed::Button(CONNECT) if self.accepted && self.phase == Phase::Idle => {
                self.connect();
            }
            _ => {}
        }
        true
    }

    fn connect(&mut self) {
        let Some(code) = normalize_code(&self.dialog.code()) else {
            self.error = Some(format!("Type all {CODE_LEN} digits of the code."));
            return;
        };
        let Some(desktop) = &self.desktop else {
            return;
        };
        self.error = None;
        self.phase = Phase::Checking;
        let links = Links {
            media: Some(desktop.local.media.clone()),
            desktop: Some(desktop.local.desktop.clone()),
        };
        let config = self.config.clone();
        let events = self.events.0.clone();
        let mut stop = self.stop.subscribe();
        self.runtime.spawn(async move {
            let credential = match session::redeem(&config, &code).await {
                Ok(credential) => credential,
                Err(e) => {
                    warn!("code refused: {e}");
                    let _ = events.send(Event::Failed(session::failure_text(&e)));
                    return;
                }
            };
            info!(agent_id = %credential.agent_id, "code accepted");
            let (status, connection) = watch::channel(agent::core::initial_status(Some(
                credential.agent_id.clone(),
            )));
            let _ = events.send(Event::Started(connection));
            let error = tokio::select! {
                error = session::serve(&credential, links, status) => error,
                // Dropping the session closes its connection.
                _ = stop.wait_for(|stop| *stop) => return,
            };
            warn!("session stopped: {error:#}");
            let _ = events.send(Event::Failed(format!("The session stopped: {error}")));
        });
    }

    /// What the session and the desktop reported since the last turn.
    fn poll(&mut self) {
        while let Ok(event) = self.events.1.try_recv() {
            match event {
                Event::Started(connection) => {
                    self.phase = Phase::Session;
                    self.connection = Some(connection);
                }
                Event::Failed(reason) => {
                    self.phase = Phase::Idle;
                    self.connection = None;
                    self.error = Some(reason);
                    // Typing the code again starts from empty boxes.
                    self.dialog.clear_code();
                }
            }
        }
        if let Some(desktop) = &self.desktop {
            while let Ok(event) = desktop.local.ui.try_recv() {
                match event {
                    UiEvent::Technicians(list) => {
                        desktop.ui.set_technicians(&list);
                        self.technicians = list;
                    }
                    UiEvent::SetClipboard(data) => desktop.ui.set_clipboard(&data),
                    UiEvent::Status(_) | UiEvent::Disconnected => {}
                }
            }
            desktop.ui.poll();
        }
        self.dialog.set(self.page());
    }

    /// The message loop. Returns when the window is closed.
    fn run(&mut self) {
        loop {
            // Sleep until there is window input, or at most 100 ms.
            // SAFETY: no handles; just waits on this thread's message queue.
            unsafe {
                MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
            }
            let mut msg = MSG::default();
            // SAFETY: standard message pump on the thread that owns the
            // windows; the dialog sees its field's keys first.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if let Some(desktop) = &self.desktop {
                        if desktop.ui.is_kill_switch(&msg) {
                            desktop.ui.kill_switch("hotkey");
                            continue;
                        }
                    }
                    if !self.dialog.pre_translate(&msg) {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            for pressed in self.dialog.events() {
                if !self.handle(pressed) {
                    info!("window closed");
                    return;
                }
            }
            self.poll();
        }
    }
}

pub fn run() {
    common::logging::init_file(&std::env::temp_dir().join("TetanusRMM-Assist.log"));
    // Physical pixels everywhere, as in the agent's helper: injected pointer
    // positions must land where the viewer clicked. Before any window.
    // SAFETY: process-wide setting, no pointers.
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        warn!("setting DPI awareness: {e}");
    }

    let elevated = is_elevated();
    if !elevated && !std::env::args().any(|arg| arg == NO_ELEVATE) && relaunch_elevated() {
        // The elevated copy takes over.
        return;
    }
    info!(
        version = agent::core::version(),
        elevated, "quick assist starting"
    );

    let Some(config) = crate::load_config() else {
        message_box(
            "This copy of quick assist is not set up for a support server.\n\n\
             Download it again from the link the person supporting you gave you.",
        );
        return;
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => return message_box(&format!("Quick assist could not start: {e}")),
    };
    // The company's branding comes with the download: the warning is shown
    // before anything is fetched.
    look::use_branding(config.branding.clone());
    let options = Options {
        minimize: true,
        ..Default::default()
    };
    let dialog = match Dialog::open(warning_page(WARNING_DELAY_SECS), options) {
        Ok(dialog) => dialog,
        Err(e) => return message_box(&format!("Quick assist could not open its window: {e}")),
    };
    let mut app = App {
        dialog,
        elevated,
        config,
        runtime,
        warned: Instant::now(),
        accepted: false,
        desktop: None,
        phase: Phase::Idle,
        error: None,
        events: mpsc::channel(),
        connection: None,
        stop: watch::channel(false).0,
        technicians: Vec::new(),
    };
    app.run();
    // End the session, and stay just long enough for the server to hear of
    // it: otherwise the technician watches a frozen picture until the
    // connection times out.
    let App {
        dialog,
        runtime,
        desktop,
        stop,
        ..
    } = app;
    drop(dialog);
    let _ = stop.send(true);
    runtime.block_on(tokio::time::sleep(CLOSE_GRACE));
    drop(desktop);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    info!("quick assist exiting");
}
