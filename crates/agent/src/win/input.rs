//! Input injection on the desktop the calling thread is attached to (the
//! session-0 service cannot inject into a user's session). The system
//! helper uses this as SYSTEM, so input reaches elevated windows and,
//! since its thread follows the input desktop (see `super::desktop`), the
//! logon screen, the lock screen and UAC prompts; the session helper uses
//! it as a fallback.
//!
//! Both helpers are per-monitor DPI aware (see `helper::run`), so the virtual
//! desktop metrics are in physical pixels, the same space DXGI reports
//! monitor positions in and the viewer's coordinates are based on. Mapping
//! is in `crate::input` so it is tested on every platform.
//!
//! Limits: from the session helper, which runs as the user on their own
//! desktop, injection does not reach the secure desktop, and Windows
//! (UIPI) drops input aimed at elevated windows. Ctrl+Alt+Del cannot be
//! injected by anyone this way (see `super::sas`).

use std::time::{Duration, Instant};

use protocol::input::{scancode, InputEvent, MouseButton};
use protocol::media::MonitorInfo;
use tracing::{debug, warn};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use super::capture;
use crate::input::{absolute, monitor_pixel, VirtualDesktop};

/// How long a cached monitor layout is trusted.
const LAYOUT_TTL: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Injector {
    monitors: Vec<MonitorInfo>,
    fetched: Option<Instant>,
}

impl Injector {
    /// Inject `event`. `false` if Windows did not take it: this thread is
    /// not on the desktop receiving input.
    pub fn inject(&mut self, event: InputEvent) -> bool {
        let inputs = match event {
            InputEvent::MouseMove { monitor, x, y } => {
                let Some(m) = self.monitor(monitor) else {
                    debug!(monitor, "pointer move for an unknown monitor");
                    return true;
                };
                let (nx, ny) = absolute(monitor_pixel(&m, x, y), virtual_desktop());
                vec![mouse(
                    nx,
                    ny,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )]
            }
            InputEvent::MouseButton { button, down } => {
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                };
                vec![mouse(0, 0, u32::from(data), flags)]
            }
            InputEvent::Wheel { dx, dy } => {
                let mut v = Vec::new();
                if dy != 0 {
                    v.push(mouse(0, 0, i32::from(dy) as u32, MOUSEEVENTF_WHEEL));
                }
                if dx != 0 {
                    v.push(mouse(0, 0, i32::from(dx) as u32, MOUSEEVENTF_HWHEEL));
                }
                v
            }
            InputEvent::Key { scancode, down } => {
                let mut flags = KEYEVENTF_SCANCODE;
                if scancode::is_extended(scancode) {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                vec![key(scancode & 0xFF, flags)]
            }
        };
        if inputs.is_empty() {
            return true;
        }
        // SAFETY: `inputs` is a valid slice of initialised INPUT structs.
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            // Typically another desktop (UAC, lock screen) is active.
            debug!(
                ?event,
                error = %windows::core::Error::from_thread(),
                "SendInput was blocked"
            );
            return false;
        }
        true
    }

    /// The monitor with `id`, refreshing the cached layout when it is stale
    /// or does not know the id (monitors plugged in or rearranged).
    fn monitor(&mut self, id: u32) -> Option<MonitorInfo> {
        let stale = self.fetched.is_none_or(|t| t.elapsed() > LAYOUT_TTL);
        if stale || !self.monitors.iter().any(|m| m.id == id) {
            match capture::monitors() {
                Ok(list) => {
                    self.monitors = list;
                    self.fetched = Some(Instant::now());
                }
                Err(e) => warn!("listing monitors: {e}"),
            }
        }
        self.monitors.iter().find(|m| m.id == id).cloned()
    }
}

fn virtual_desktop() -> VirtualDesktop {
    // SAFETY: plain metric queries.
    unsafe {
        VirtualDesktop {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
