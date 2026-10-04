//! The tray icon's flyout, in place of a menu: which agent this is and
//! whether it is connected, who is in a remote session, the way to end
//! them all, About, and Quit (which is not the user's to press).
//!
//! It is a layered window (rounded corners and a soft shadow) that opens
//! above the tray icon and closes when anything else is clicked, when it
//! loses the keyboard, or with Esc. (For its first moments it holds on
//! instead: a notification leaving the screen as it opens would otherwise
//! take it down with it.) Like the session bar it lives on the
//! helper's UI thread, and only records what was picked for the tray's
//! loop to act on ([`Flyout::take_actions`]).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use brand::theme::palette;
use brand::{icons, Rgb};
use protocol::ipc::AgentStatus;
use tracing::debug;
use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetForegroundWindow, LoadCursorW,
    RegisterClassW, SetForegroundWindow, SetWindowPos, ShowWindow, UpdateLayeredWindow,
    HWND_TOPMOST, IDC_ARROW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    ULW_ALPHA, WM_ACTIVATE, WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::consent::initial;
use super::ui::canvas::{with_painter, Align, Canvas, Painter, Rect, Surface, Text};
use super::ui::look::Look;

/// `WM_MOUSELEAVE` (from winuser.h).
const WM_MOUSELEAVE: u32 = 0x02A3;

/// The flyout's width, the room around it for its shadow, and the gap
/// between it and the tray icon.
const WIDTH: f32 = 314.0;
const SHADOW: f32 = 16.0;
const GAP: f32 = 4.0;
const RADIUS: f32 = 8.0;
/// A row's height, and how far rows are inset from the edges.
const ROW_H: f32 = 37.0;
const INSET: f32 = 5.0;
/// How long after opening it insists on being the active window.
const SETTLE: Duration = Duration::from_millis(600);

/// What the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    EndSessions,
    About,
}

struct Panel {
    hwnd: HWND,
    open: bool,
    opened: Instant,
    status: AgentStatus,
    technicians: Vec<String>,
    hover: Option<Action>,
    tracking: bool,
    /// The rows that can be picked, as last drawn.
    rows: Vec<(Action, Rect)>,
    actions: Vec<Action>,
    /// The tray icon's rectangle, in screen pixels.
    anchor: RECT,
    dpi: u32,
}

thread_local! {
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
}

impl Panel {
    /// Lay the flyout out, drawing it if there is a canvas. Returns its
    /// height (without the shadow) and the rows that can be picked.
    fn layout(&self, p: &Painter, c: Option<&Canvas>, look: &Look) -> (f32, Vec<(Action, Rect)>) {
        let t = &look.theme;
        let mut rows = Vec::new();
        let muted = Text::new(12.5, t.text_muted).middle();
        let item = Text::new(13.5, t.text).middle();
        let pad = 17.0;
        let mut y = 0.0;

        // Which agent, and how it is doing.
        if let Some(c) = c {
            match &look.logo {
                Some(logo) => c.logo(logo, pad, pad, 33.0),
                None => c.mark(pad, pad, 33.0, t.accent),
            }
            let x = pad + 33.0 + 12.0;
            c.text(
                &look.agent_name(),
                Rect::new(x, pad - 1.0, WIDTH - x - pad, 20.0),
                &Text::new(15.0, t.text).semibold().middle(),
            );
            let dot = match (&self.status.agent_id, self.status.connected) {
                (Some(_), true) => t.ok,
                _ => t.text_muted,
            };
            c.circle(x + 3.5, pad + 26.0, 3.5, dot);
            c.text(
                &format!("{} \u{b7} v{}", self.status.state(), self.status.version),
                Rect::new(x + 13.0, pad + 16.0, WIDTH - x - pad, 20.0),
                &muted,
            );
        }
        y += pad + 33.0 + pad;
        let divider = |c: Option<&Canvas>, y: f32| {
            if let Some(c) = c {
                c.rect(Rect::new(0.0, y, WIDTH, 1.0), t.line);
            }
        };
        divider(c, y);
        y += 1.0;

        // Who is in a session.
        if let Some(c) = c {
            c.text(
                "Remote sessions",
                Rect::new(pad, y + 8.0, 200.0, 22.0),
                &muted,
            );
        }
        y += 34.0;
        if self.technicians.is_empty() {
            if let Some(c) = c {
                c.text("Nobody is connected", Rect::new(pad, y, 250.0, 26.0), &item);
            }
            y += 30.0;
        }
        for name in &self.technicians {
            if let Some(c) = c {
                c.circle(pad + 12.5, y + 13.0, 12.5, t.avatar);
                c.text(
                    &initial(name),
                    Rect::new(pad, y + 0.5, 25.0, 25.0),
                    &Text::new(11.5, t.on_avatar).semibold().centered(),
                );
                let x = pad + 25.0 + 10.0;
                let w = p.measure(name, &item, 400.0).0.min(WIDTH - x - 110.0);
                c.text(name, Rect::new(x, y, w, 26.0), &item);
                c.text(
                    " \u{b7} controlling",
                    Rect::new(x + w, y, 100.0, 26.0),
                    &Text {
                        color: t.text_muted,
                        ..item
                    },
                );
                c.circle(WIDTH - pad - 3.5, y + 13.0, 3.5, t.live_text);
            }
            y += 34.0;
        }

        // End them all.
        let ending = !self.technicians.is_empty();
        let row = Rect::new(INSET, y, WIDTH - 2.0 * INSET, ROW_H);
        if let Some(c) = c {
            let (shade, tint) = if ending {
                (t.live_text, 0.12)
            } else {
                (t.text_muted, 0.0)
            };
            if ending {
                let more = if self.hover == Some(Action::EndSessions) {
                    0.08
                } else {
                    0.0
                };
                c.round(row, 5.0, t.body.mix(palette::LIVE, tint + more));
            }
            c.icon(&icons::END, pad, y + 10.5, 16.0, shade);
            let style = Text {
                color: shade,
                ..item
            };
            c.text(
                "End all remote sessions",
                Rect::new(pad + 27.0, y, 200.0, ROW_H),
                &style,
            );
            c.text(
                "Ctrl+F12",
                Rect::new(row.x, y, row.w - 10.0, ROW_H),
                &Text::new(12.0, shade).middle().align(Align::Right),
            );
        }
        if ending {
            rows.push((Action::EndSessions, row));
        }
        y += ROW_H + 8.0;
        divider(c, y);
        y += 1.0 + 5.0;

        // About, and Quit.
        let about = Rect::new(INSET, y, WIDTH - 2.0 * INSET, ROW_H);
        if let Some(c) = c {
            if self.hover == Some(Action::About) {
                c.round(about, 5.0, t.control_hover);
            }
            c.icon(&icons::INFO, pad, y + 10.5, 16.0, t.text);
            c.text(
                &format!("About {}", look.agent_name()),
                Rect::new(pad + 27.0, y, WIDTH - pad * 2.0 - 27.0, ROW_H),
                &item,
            );
        }
        rows.push((Action::About, about));
        y += ROW_H;
        if let Some(c) = c {
            // Not the user's to press: the administrator's.
            c.icon(&icons::LOCK, pad, y + 12.0, 16.0, t.text_muted);
            c.text(
                "Quit",
                Rect::new(pad + 27.0, y + 3.0, 200.0, 20.0),
                &Text {
                    color: t.text_muted,
                    ..item
                },
            );
            c.text(
                "Managed by your administrator",
                Rect::new(pad + 27.0, y + 21.0, 250.0, 18.0),
                &Text::new(11.5, t.text_muted).middle(),
            );
        }
        y += 46.0;
        if look.company.is_some() {
            divider(c, y);
            if let Some(c) = c {
                c.text(
                    &format!("Powered by {}", brand::PRODUCT),
                    Rect::new(0.0, y + 1.0, WIDTH, 27.0),
                    &Text::new(11.5, t.text_muted).centered(),
                );
            }
            y += 28.0;
        }
        (y + 3.0, rows)
    }

    fn render(&mut self) {
        if !self.open {
            return;
        }
        // SAFETY: a plain query of our own window.
        self.dpi = match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 96,
            dpi => dpi,
        };
        let look = Look::current();
        let px = |dips: f32| (dips * self.dpi as f32 / 96.0).round() as i32;
        let shown = with_painter(|p| -> Result<Vec<(Action, Rect)>> {
            let (height, _) = self.layout(p, None, &look);
            let surface = Surface::new(px(WIDTH + 2.0 * SHADOW), px(height + 2.0 * SHADOW))?;
            let mut rows = Vec::new();
            p.draw(&surface, self.dpi, |c| {
                let panel = Rect::new(SHADOW, SHADOW, WIDTH, height);
                // A soft shadow: rings fading out, a little lower than the
                // panel.
                for ring in 0..12 {
                    let spread = ring as f32 + 1.0;
                    let alpha = 0.022 * (12.0 - ring as f32) / 12.0;
                    let mut shadow = panel.inflate(spread);
                    shadow.y += 3.0;
                    c.round_alpha(shadow, RADIUS + spread, Rgb::BLACK, alpha);
                }
                c.round(panel, RADIUS, look.theme.body);
                c.outline(panel, RADIUS, look.theme.control_line, 1.0);
                c.clipped(panel, || {
                    c.offset(SHADOW, SHADOW, || {
                        rows = self.layout(p, Some(c), &look).1;
                    });
                });
            })?;
            // Above the tray icon (or below it, with the taskbar on top),
            // kept on the icon's monitor.
            let anchor = self.anchor;
            let centre = POINT {
                x: (anchor.left + anchor.right) / 2,
                y: (anchor.top + anchor.bottom) / 2,
            };
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            // SAFETY: an out-parameter of the size it says.
            unsafe {
                let monitor = MonitorFromPoint(centre, MONITOR_DEFAULTTONEAREST);
                let _ = GetMonitorInfoW(monitor, &mut info);
            }
            let work = info.rcWork;
            let (w, h) = (surface.width, surface.height);
            let (shadow, gap) = (px(SHADOW), px(GAP));
            let above = anchor.top - gap - (h - shadow);
            let y = if above >= work.top - shadow {
                above
            } else {
                anchor.bottom + gap - shadow
            };
            let x = (centre.x - w / 2).clamp(
                work.left - shadow + gap,
                (work.right - w + shadow - gap).max(work.left - shadow + gap),
            );
            let at = POINT {
                x,
                y: y.min(work.bottom - h + shadow - gap),
            };
            let size = SIZE { cx: w, cy: h };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            // SAFETY: the surface, with its premultiplied alpha, becomes
            // the whole of our own layered window.
            unsafe {
                UpdateLayeredWindow(
                    self.hwnd,
                    None,
                    Some(&at),
                    Some(&size),
                    Some(surface.hdc()),
                    Some(&POINT::default()),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                )?;
            }
            Ok(rows)
        });
        match shown {
            Some(Ok(rows)) => self.rows = rows,
            Some(Err(e)) => debug!("showing the tray flyout: {e}"),
            None => {}
        }
    }

    fn row_at(&self, lparam: LPARAM) -> Option<Action> {
        let k = 96.0 / self.dpi.max(1) as f32;
        let at = (
            (lparam.0 & 0xFFFF) as i16 as f32 * k - SHADOW,
            ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 * k - SHADOW,
        );
        self.rows
            .iter()
            .find(|(_, rect)| rect.contains(at))
            .map(|(action, _)| *action)
    }

    fn close(&mut self) {
        if self.open {
            self.open = false;
            self.hover = None;
            // SAFETY: our own window.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Showing, hiding and moving the window send it messages while the
    // panel is already borrowed: those are not for it.
    let handled = PANEL.with(|panel| {
        let Ok(mut panel) = panel.try_borrow_mut() else {
            return false;
        };
        let Some(panel) = panel.as_mut() else {
            return false;
        };
        match msg {
            // Something else was clicked, or took the keyboard. (Just after
            // opening, `Flyout::tick` takes it back instead.)
            WM_ACTIVATE if wparam.0 & 0xFFFF == 0 => {
                if panel.opened.elapsed() >= SETTLE {
                    panel.close();
                }
            }
            WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => panel.close(),
            WM_MOUSEMOVE | WM_MOUSELEAVE => {
                let hover = (msg == WM_MOUSEMOVE)
                    .then(|| panel.row_at(lparam))
                    .flatten();
                if msg == WM_MOUSELEAVE {
                    panel.tracking = false;
                } else if !panel.tracking {
                    panel.tracking = true;
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: a valid structure for our own window.
                    unsafe {
                        let _ = TrackMouseEvent(&mut track);
                    }
                }
                if hover != panel.hover {
                    panel.hover = hover;
                    panel.render();
                }
            }
            WM_LBUTTONUP => {
                if let Some(action) = panel.row_at(lparam) {
                    panel.actions.push(action);
                    panel.close();
                }
            }
            _ => return false,
        }
        true
    });
    if handled {
        return LRESULT(0);
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub struct Flyout {
    hwnd: HWND,
}

impl Flyout {
    /// Create the (hidden) flyout on the calling thread, which must run a
    /// message loop.
    pub fn new(status: AgentStatus) -> Result<Self> {
        // SAFETY: registering a class and creating a window with static
        // strings and a valid window procedure, on this thread.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("RmmAgentTrayFlyout");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            });
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
                class,
                w!("Remote support"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )?
        };
        PANEL.set(Some(Panel {
            hwnd,
            open: false,
            opened: Instant::now(),
            status,
            technicians: Vec::new(),
            hover: None,
            tracking: false,
            rows: Vec::new(),
            actions: Vec::new(),
            anchor: RECT::default(),
            dpi: 96,
        }));
        Ok(Self { hwnd })
    }

    /// Open the flyout by the tray icon at `anchor` (screen pixels), or
    /// close it if it is open.
    pub fn toggle(&self, anchor: RECT) {
        let hwnd = self.hwnd;
        let opened = PANEL.with_borrow_mut(|panel| {
            let Some(panel) = panel.as_mut() else {
                return false;
            };
            if panel.open {
                panel.close();
                return false;
            }
            panel.open = true;
            panel.opened = Instant::now();
            panel.anchor = anchor;
            // On the icon's monitor first, so that its scaling applies.
            // SAFETY: placing and showing our own window.
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    anchor.left,
                    anchor.top,
                    1,
                    1,
                    SWP_NOACTIVATE,
                );
            }
            panel.render();
            // SAFETY: as above.
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
                );
            }
            true
        });
        if opened {
            // It closes itself when it stops being the active window, so it
            // has to become it. Outside the borrow: this sends messages.
            // SAFETY: our own window.
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    /// Call every turn of the message loop. While the flyout is open it
    /// must be the active window, or nothing would tell it to close: so
    /// just after opening it takes the place back if something else took
    /// it, and after that it closes.
    pub fn tick(&self) {
        let hwnd = self.hwnd;
        let retake = PANEL.with_borrow_mut(|panel| {
            let Some(panel) = panel.as_mut().filter(|panel| panel.open) else {
                return false;
            };
            // SAFETY: a plain query.
            if unsafe { GetForegroundWindow() } == hwnd {
                return false;
            }
            if panel.opened.elapsed() < SETTLE {
                return true;
            }
            panel.close();
            false
        });
        if retake {
            // Outside the borrow: this sends messages.
            // SAFETY: our own window.
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    /// What the flyout shows changed: redraw it if it is open.
    pub fn update(&self, status: &AgentStatus, technicians: &[String]) {
        PANEL.with_borrow_mut(|panel| {
            if let Some(panel) = panel.as_mut() {
                panel.status = status.clone();
                // Someone with several viewers open is listed once.
                panel.technicians.clear();
                for name in technicians {
                    if !panel.technicians.contains(name) {
                        panel.technicians.push(name.clone());
                    }
                }
                panel.render();
            }
        });
    }

    /// Follow a change of look (the branding, light or dark).
    pub fn restyle(&self) {
        PANEL.with_borrow_mut(|panel| {
            if let Some(panel) = panel.as_mut() {
                panel.render();
            }
        });
    }

    /// What the user picked since the last call.
    pub fn take_actions(&self) -> Vec<Action> {
        PANEL.with_borrow_mut(|panel| {
            panel
                .as_mut()
                .map(|panel| std::mem::take(&mut panel.actions))
                .unwrap_or_default()
        })
    }
}

impl Drop for Flyout {
    fn drop(&mut self) {
        PANEL.set(None);
        // SAFETY: our own window, on its thread.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
