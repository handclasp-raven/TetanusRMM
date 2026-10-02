//! The quick assist window, and everything around it on Windows.
//!
//! Start-up: ask for elevation (so the technician's input can reach
//! elevated windows), carrying on without it if the user says no. Then one
//! small window with two pages:
//!
//! 1. The scam warning. It cannot be accepted for
//!    [`WARNING_DELAY_SECS`] seconds: the button is disabled and counts
//!    down. Nothing touches the network, the screen or the clipboard until
//!    it has been accepted.
//! 2. The code. Once the server accepts it, this process is the session's
//!    agent ([`crate::session`]) with the desktop in-process
//!    (`agent::win::local`), until the window is closed.
//!
//! The window procedure only records what happened ([`Action`]); the
//! message loop in [`App::run`] acts on it, alongside what the session and
//! the desktop report. That loop is also where the Ctrl+F12 kill switch,
//! clipboard sync and the on-screen indicator live, as in the agent's
//! helper.

use std::cell::RefCell;
use std::sync::mpsc;

use agent::win::helper::{DesktopUi, UiEvent};
use agent::win::local::{self, LocalDesktop};
use protocol::assist::{
    accept_button, normalize_code, AssistConfig, CODE_LEN, SCAM_WARNING, WARNING_DELAY_SECS,
};
use protocol::ipc::AgentStatus;
use tokio::sync::watch;
use tracing::{info, warn};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE, DEFAULT_CHARSET, FF_SWISS,
    FW_NORMAL, FW_SEMIBOLD, HBRUSH, HFONT, OUT_DEFAULT_PRECIS, VARIABLE_PITCH,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetSystemMetrics, GetWindowTextW, IsDialogMessageW, KillTimer, LoadCursorW, MessageBoxW,
    MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage,
    BS_DEFPUSHBUTTON, BS_PUSHBUTTON, ES_CENTER, ES_NUMBER, HMENU, HWND_NOTOPMOST, HWND_TOPMOST,
    IDC_ARROW, MB_ICONERROR, MB_OK, MSG, PM_REMOVE, QS_ALLINPUT, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOW, SW_SHOWNORMAL, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_QUIT, WM_SETFONT, WM_TIMER, WNDCLASSW,
    WS_BORDER, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP,
};

use crate::session::{self, Links, Phase};

const TITLE: &str = "TetanusRMM Quick Assist";

/// Passed to the elevated copy of this program, so that it does not ask
/// for elevation again.
const NO_ELEVATE: &str = "--no-elevate";

// Control ids (WM_COMMAND). 1 and 2 are what the dialog manager sends for
// Enter and Escape.
const ID_ENTER: usize = 1;
const ID_ACCEPT: usize = 100;
const ID_CLOSE: usize = 101;
const ID_CONNECT: usize = 102;
const ID_END: usize = 103;

/// How long the connection gets to tell the server it is closing.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// The countdown timer.
const TIMER: usize = 1;

/// `EM_SETLIMITTEXT`: the most characters an edit control takes.
const EM_SETLIMITTEXT: u32 = 0x00C5;

/// `EM_SETSEL`: select a range of an edit control's text (0 to -1: all).
const EM_SETSEL: u32 = 0x00B1;

/// Client area at 96 DPI; scaled for the system's DPI.
const WIDTH: i32 = 460;
const HEIGHT: i32 = 290;
const MARGIN: i32 = 20;

/// What the window procedure saw, for the message loop to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// "I understand" was pressed.
    Accept,
    /// A second of the warning's delay passed.
    Tick,
    /// "Connect" was pressed.
    Connect,
    /// Enter was pressed with no button focused.
    Enter,
    /// A close button was pressed.
    Close,
}

thread_local! {
    static ACTIONS: RefCell<Vec<Action>> = const { RefCell::new(Vec::new()) };
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let action = match msg {
        WM_COMMAND => match wparam.0 & 0xffff {
            ID_ACCEPT => Some(Action::Accept),
            ID_CONNECT => Some(Action::Connect),
            ID_CLOSE | ID_END => Some(Action::Close),
            ID_ENTER => Some(Action::Enter),
            // Escape (2) does nothing: the window is closed on purpose.
            _ => None,
        },
        WM_TIMER if wparam.0 == TIMER => Some(Action::Tick),
        WM_CLOSE => Some(Action::Close),
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    };
    if let Some(action) = action {
        ACTIONS.with_borrow_mut(|actions| actions.push(action));
    }
    LRESULT(0)
}

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

struct Controls {
    // The warning page.
    warning: HWND,
    close: HWND,
    accept: HWND,
    // The session page.
    prompt: HWND,
    code: HWND,
    connect: HWND,
    status: HWND,
    note: HWND,
    end: HWND,
}

impl Controls {
    fn warning_page(&self) -> [HWND; 3] {
        [self.warning, self.close, self.accept]
    }

    fn session_page(&self) -> [HWND; 6] {
        [
            self.prompt,
            self.code,
            self.connect,
            self.status,
            self.note,
            self.end,
        ]
    }
}

fn set_text(hwnd: HWND, text: &str) {
    // SAFETY: our own window, an owned string.
    unsafe {
        let _ = SetWindowTextW(hwnd, &HSTRING::from(text));
    }
}

fn enable(hwnd: HWND, enabled: bool) {
    // SAFETY: our own window.
    unsafe {
        let _ = EnableWindow(hwnd, enabled);
    }
}

fn show(hwnd: HWND, visible: bool) {
    // SAFETY: our own window.
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
}

fn font(px: i32, weight: i32) -> HFONT {
    // SAFETY: static face name. The font lives as long as the process.
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            u32::from(VARIABLE_PITCH.0 | FF_SWISS.0),
            w!("Segoe UI"),
        )
    }
}

/// The window and its controls, laid out for the system's DPI.
struct Window {
    hwnd: HWND,
    controls: Controls,
}

impl Window {
    fn new(elevated: bool) -> windows::core::Result<Self> {
        // SAFETY: plain query.
        let dpi = match unsafe { GetDpiForSystem() } {
            0 => 96,
            dpi => dpi as i32,
        };
        let scale = |px: i32| px * dpi / 96;
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;

        // SAFETY: registering a class and creating windows with static or
        // owned strings and a valid window procedure, on this thread.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("TetanusRmmQuickAssist");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                // The dialog colour, which the controls draw on too.
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
                ..Default::default()
            };
            RegisterClassW(&wc);

            let mut frame = RECT {
                left: 0,
                top: 0,
                right: scale(WIDTH),
                bottom: scale(HEIGHT),
            };
            AdjustWindowRectEx(&mut frame, style, false, WINDOW_EX_STYLE(0))?;
            let (width, height) = (frame.right - frame.left, frame.bottom - frame.top);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                &HSTRING::from(TITLE),
                style,
                (GetSystemMetrics(SM_CXSCREEN) - width) / 2,
                (GetSystemMetrics(SM_CYSCREEN) - height) / 2,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            )?;

            let text_font = font(scale(15), FW_NORMAL.0 as i32);
            let warning_font = font(scale(16), FW_SEMIBOLD.0 as i32);
            let code_font = font(scale(30), FW_SEMIBOLD.0 as i32);
            let child = |class: PCWSTR,
                         text: &str,
                         style: u32,
                         id: usize,
                         rect: (i32, i32, i32, i32),
                         font: HFONT|
             -> windows::core::Result<HWND> {
                let control = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class,
                    &HSTRING::from(text),
                    WINDOW_STYLE(WS_CHILD.0 | style),
                    scale(rect.0),
                    scale(rect.1),
                    scale(rect.2),
                    scale(rect.3),
                    Some(hwnd),
                    Some(HMENU(id as *mut _)),
                    Some(instance.into()),
                    None,
                )?;
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
                Ok(control)
            };
            let inner = WIDTH - 2 * MARGIN;
            let button = WS_TABSTOP.0 | BS_PUSHBUTTON as u32;
            let (accept_label, _) = accept_button(WARNING_DELAY_SECS);

            let controls = Controls {
                warning: child(
                    w!("STATIC"),
                    SCAM_WARNING,
                    0,
                    0,
                    (MARGIN, MARGIN, inner, 200),
                    warning_font,
                )?,
                close: child(
                    w!("BUTTON"),
                    "Close",
                    button,
                    ID_CLOSE,
                    (MARGIN, 236, 130, 34),
                    text_font,
                )?,
                accept: child(
                    w!("BUTTON"),
                    &accept_label,
                    button,
                    ID_ACCEPT,
                    (WIDTH - MARGIN - 170, 236, 170, 34),
                    text_font,
                )?,
                prompt: child(
                    w!("STATIC"),
                    "Type the six-digit code the person supporting you gave you.",
                    0,
                    0,
                    (MARGIN, MARGIN, inner, 40),
                    text_font,
                )?,
                code: child(
                    w!("EDIT"),
                    "",
                    WS_TABSTOP.0 | WS_BORDER.0 | (ES_NUMBER | ES_CENTER) as u32,
                    0,
                    ((WIDTH - 200) / 2, 64, 200, 46),
                    code_font,
                )?,
                connect: child(
                    w!("BUTTON"),
                    "Connect",
                    WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
                    ID_CONNECT,
                    ((WIDTH - 120) / 2, 120, 120, 34),
                    text_font,
                )?,
                status: child(w!("STATIC"), "", 0, 0, (MARGIN, 168, inner, 62), text_font)?,
                note: child(
                    w!("STATIC"),
                    if elevated {
                        ""
                    } else {
                        "Running without administrator rights: your supporter cannot \
                         control administrator windows."
                    },
                    0,
                    0,
                    (MARGIN, 240, inner - 130, 38),
                    font(scale(12), FW_NORMAL.0 as i32),
                )?,
                end: child(
                    w!("BUTTON"),
                    "End session",
                    button,
                    ID_END,
                    (WIDTH - MARGIN - 120, 236, 120, 34),
                    text_font,
                )?,
            };
            SendMessageW(
                controls.code,
                EM_SETLIMITTEXT,
                Some(WPARAM(CODE_LEN)),
                Some(LPARAM(0)),
            );
            Ok(Self { hwnd, controls })
        }
    }

    /// Show the warning page, which cannot be accepted yet.
    fn show_warning(&self) {
        for control in self.controls.session_page() {
            show(control, false);
        }
        enable(self.controls.accept, false);
        for control in self.controls.warning_page() {
            show(control, true);
        }
        // SAFETY: our own windows, on their thread.
        unsafe {
            let _ = SetFocus(Some(self.controls.close));
            SetTimer(Some(self.hwnd), TIMER, 1000, None);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            // Windows only lets the program the user last used take the
            // foreground. Started some other way, at least be on top.
            if !SetForegroundWindow(self.hwnd).as_bool() {
                for order in [HWND_TOPMOST, HWND_NOTOPMOST] {
                    let _ = SetWindowPos(
                        self.hwnd,
                        Some(order),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }
    }

    /// The warning's delay has `remaining` seconds left.
    fn set_countdown(&self, remaining: u32) {
        let (label, enabled) = accept_button(remaining);
        set_text(self.controls.accept, &label);
        enable(self.controls.accept, enabled);
        if enabled {
            // SAFETY: our own timer.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), TIMER);
            }
        }
    }

    fn show_session(&self) {
        for control in self.controls.warning_page() {
            show(control, false);
        }
        for control in self.controls.session_page() {
            show(control, true);
        }
        // SAFETY: our own window.
        unsafe {
            let _ = SetFocus(Some(self.controls.code));
        }
    }

    /// Whether a code can be typed and sent.
    fn set_code_entry(&self, enabled: bool) {
        enable(self.controls.code, enabled);
        enable(self.controls.connect, enabled);
        if enabled {
            // Back in the field with what was typed selected, so that
            // typing the code again replaces it.
            // SAFETY: our own window.
            unsafe {
                let _ = SetFocus(Some(self.controls.code));
                SendMessageW(
                    self.controls.code,
                    EM_SETSEL,
                    Some(WPARAM(0)),
                    Some(LPARAM(-1)),
                );
            }
        }
    }

    fn typed_code(&self) -> String {
        let mut buffer = [0u16; 32];
        // SAFETY: our own window; the buffer's length is passed with it.
        let len = unsafe { GetWindowTextW(self.controls.code, &mut buffer) };
        String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)])
    }
}

/// The desktop, once the warning has been accepted.
struct Desktop {
    local: LocalDesktop,
    ui: DesktopUi,
}

struct App {
    window: Window,
    config: AssistConfig,
    runtime: tokio::runtime::Runtime,
    /// Seconds of the warning's delay left; the warning is accepted only
    /// at zero.
    remaining: u32,
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
    /// The status line as last written.
    shown: String,
}

impl App {
    fn handle(&mut self, action: Action) -> bool {
        match action {
            Action::Close => return false,
            Action::Tick if !self.accepted => {
                self.remaining = self.remaining.saturating_sub(1);
                self.window.set_countdown(self.remaining);
            }
            // Only a press that comes after the whole delay counts.
            Action::Accept if !self.accepted && self.remaining == 0 => {
                info!("scam warning accepted");
                self.accepted = true;
                let _runtime = self.runtime.enter();
                let local = local::start();
                let ui = DesktopUi::new(local.ctl.clone());
                self.desktop = Some(Desktop { local, ui });
                self.window.show_session();
            }
            Action::Connect | Action::Enter if self.accepted && self.phase == Phase::Idle => {
                self.connect();
            }
            _ => {}
        }
        true
    }

    fn connect(&mut self) {
        let Some(code) = normalize_code(&self.window.typed_code()) else {
            self.error = Some(format!("Type all {CODE_LEN} digits of the code."));
            return;
        };
        let Some(desktop) = &self.desktop else {
            return;
        };
        self.error = None;
        self.phase = Phase::Checking;
        self.window.set_code_entry(false);
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
                    self.window.set_code_entry(true);
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
        let connected = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.borrow().connected);
        let text = match (&self.error, self.phase) {
            (Some(error), Phase::Idle) => error.clone(),
            _ => session::status_text(self.phase, connected, &self.technicians),
        };
        if text != self.shown {
            set_text(self.window.controls.status, &text);
            self.shown = text;
        }
    }

    /// The message loop. Returns when the window is closed.
    fn run(&mut self) {
        self.window.show_warning();
        loop {
            // Sleep until there is window input, or at most 100 ms.
            // SAFETY: no handles; just waits on this thread's message queue.
            unsafe {
                MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
            }
            let mut msg = MSG::default();
            // SAFETY: standard message pump on the thread that owns the
            // windows; the dialog manager handles Tab and Enter.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    if let Some(desktop) = &self.desktop {
                        if desktop.ui.is_kill_switch(&msg) {
                            desktop.ui.kill_switch("hotkey");
                            continue;
                        }
                    }
                    if !IsDialogMessageW(self.window.hwnd, &msg).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            for action in ACTIONS.take() {
                if !self.handle(action) {
                    info!("window closed");
                    // SAFETY: our own window, on its thread. Posts WM_QUIT.
                    unsafe {
                        let _ = DestroyWindow(self.window.hwnd);
                    }
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
    let window = match Window::new(elevated) {
        Ok(window) => window,
        Err(e) => return message_box(&format!("Quick assist could not open its window: {e}")),
    };
    let mut app = App {
        window,
        config,
        runtime,
        remaining: WARNING_DELAY_SECS,
        accepted: false,
        desktop: None,
        phase: Phase::Idle,
        error: None,
        events: mpsc::channel(),
        connection: None,
        stop: watch::channel(false).0,
        technicians: Vec::new(),
        shown: String::new(),
    };
    app.run();
    // End the session, and stay just long enough for the server to hear of
    // it: otherwise the technician watches a frozen picture until the
    // connection times out.
    let App {
        runtime,
        desktop,
        stop,
        ..
    } = app;
    let _ = stop.send(true);
    runtime.block_on(tokio::time::sleep(CLOSE_GRACE));
    drop(desktop);
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    info!("quick assist exiting");
}
