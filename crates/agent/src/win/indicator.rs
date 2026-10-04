//! The on-screen session bar: at the top of the primary screen for as
//! long as a technician is connected, naming them, with a button that
//! ends the session. The tray icon is easy to miss; this is not. (Who it
//! names: `interactive::indicator_who`.)
//!
//! It starts as a bar ("**roker** is controlling this PC | End session
//! Ctrl+F12"). After ten seconds without the pointer over it, it shrinks
//! to a pill (a red dot, the name, and a cross that ends the session), so
//! that it is not in the way for the length of a session. A click on the
//! pill brings the bar back.
//!
//! It shows for `notify` and `require` sessions, the same technicians the
//! tray lists; `unattended` sessions are silent by policy.
//!
//! It must not get in the user's way, or become a way to interfere with
//! them:
//! - it takes clicks only on the bar or pill itself (a layered window with
//!   per-pixel alpha: what is transparent does not exist for the mouse),
//!   and is never activated (`WS_EX_NOACTIVATE`, `MA_NOACTIVATE`), so the
//!   window the user is typing in keeps the keyboard;
//! - all a click can do is end the remote sessions, which the technician's
//!   own injected clicks may do too;
//! - it is excluded from screen capture (`WDA_EXCLUDEFROMCAPTURE`, Windows
//!   10 2004+), so it does not cover what the technician is looking at. On
//!   older Windows it simply appears in the stream too.
//!
//! It lives on the helper's UI thread, whose message loop (the tray's)
//! dispatches its messages. Other topmost windows can cover it, so the
//! loop re-asserts it every few seconds ([`Indicator::tick`]).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use brand::theme::palette;
use brand::{icons, Rgb, Theme};
use tracing::{debug, info, warn};
use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, LoadCursorW, RegisterClassW,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, UpdateLayeredWindow, HWND_TOPMOST,
    IDC_ARROW, MA_NOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEACTIVATE,
    WM_MOUSEMOVE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use super::ui::canvas::{with_painter, Canvas, Painter, Rect, Surface, Text};
use super::ui::look::{self, Look};

/// `WM_MOUSELEAVE` (from winuser.h).
const WM_MOUSELEAVE: u32 = 0x02A3;

/// How long the bar stays before it shrinks to the pill.
const COLLAPSE_AFTER: Duration = Duration::from_secs(10);
/// How often it is put back on top of other topmost windows.
const RAISE_EVERY: Duration = Duration::from_secs(3);

/// The bar's and the pill's heights, and the pill's distance from the top.
const BAR_H: f32 = 45.0;
const PILL_H: f32 = 27.0;
const PILL_TOP: f32 = 6.0;

/// Dark whatever Windows is set to: it sits over anything.
const BACKGROUND: Rgb = Rgb::hex(0x1B2024);
const DIVIDER: Rgb = Rgb::hex(0x3A4149);
const CLOSE: Rgb = Rgb::hex(0x3A4149);
const CLOSE_HOVER: Rgb = Rgb::hex(0x4D5660);
const OPACITY: f32 = 0.97;

/// What a click lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// "End session", or the pill's cross.
    End,
    /// The rest of the pill: a click brings the bar back.
    Pill,
}

struct Bar {
    hwnd: HWND,
    /// Who is connected, and whether that is more than one.
    who: Option<(String, bool)>,
    expanded: bool,
    /// When the bar was shown, or the pointer last over it.
    active: Instant,
    raised: Instant,
    hover: Option<Part>,
    pressed: Option<Part>,
    tracking: bool,
    /// The parts as last drawn, in device-independent pixels.
    parts: Vec<(Part, Rect)>,
    dpi: u32,
    /// The user asked to end the sessions; taken by [`Indicator::take_end`].
    end: bool,
    stamp: (u64, bool, bool),
}

thread_local! {
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
}

/// The live dot: a bright centre in a darker ring.
fn live_dot(c: &Canvas, cx: f32, cy: f32) {
    c.circle(cx, cy, 6.5, palette::LIVE.mix(BACKGROUND, 0.45));
    c.circle(cx, cy, 3.5, palette::TRAY_LIVE);
}

impl Bar {
    /// Lay the bar out, drawing it if there is a canvas. Returns its size
    /// and the parts that take clicks.
    fn bar(&self, p: &Painter, c: Option<&Canvas>, look: &Look) -> (f32, f32, Vec<(Part, Rect)>) {
        let (names, plural) = self.who.clone().unwrap_or_default();
        let verb = if plural { " are" } else { " is" };
        let rest = format!("{verb} controlling this PC");
        let text = Text::new(13.0, Rgb::WHITE).middle();
        let runs = [(names.as_str(), true), (rest.as_str(), false)];
        let (text_w, _) = p.measure_runs(&runs, &text, 2000.0);
        let label = Text::new(13.0, Rgb::WHITE).semibold().middle();
        let (label_w, _) = p.measure("End session", &label, 400.0);
        let key = Text::new(11.5, Rgb::WHITE).centered();
        let cap_w = p.measure("Ctrl+F12", &key, 400.0).0 + 11.0;

        let mark_x = 14.0 + 13.0 + 10.0;
        let text_x = mark_x + 17.0 + 9.0;
        let divider_x = text_x + text_w + 14.0;
        let button = Rect::new(
            divider_x + 1.0 + 12.0,
            7.0,
            11.0 + label_w + 8.0 + cap_w + 7.0,
            31.0,
        );
        let width = button.right() + 7.0;
        if let Some(c) = c {
            // Flush with the top of the screen: only the lower corners round.
            c.round_alpha(
                Rect::new(0.0, -12.0, width, BAR_H + 12.0),
                10.0,
                BACKGROUND,
                OPACITY,
            );
            live_dot(c, 14.0 + 6.5, BAR_H / 2.0);
            let mark_y = (BAR_H - 17.0) / 2.0;
            match &look.logo {
                Some(logo) => c.logo(logo, mark_x, mark_y, 17.0),
                None => c.mark(mark_x, mark_y, 17.0, Theme::dark(look.accent).accent),
            }
            c.runs(&runs, Rect::new(text_x, 0.0, text_w, BAR_H), &text);
            c.rect(Rect::new(divider_x, 13.5, 1.0, 18.0), DIVIDER);
            let hover = self.hover == Some(Part::End);
            let fill = if hover && self.pressed == Some(Part::End) {
                palette::LIVE.mix(Rgb::BLACK, 0.18)
            } else if hover {
                palette::LIVE.mix(Rgb::WHITE, 0.10)
            } else {
                palette::LIVE
            };
            c.round(button, 5.0, fill);
            c.text(
                "End session",
                Rect::new(button.x + 11.0, button.y, label_w, button.h),
                &label,
            );
            let cap = Rect::new(button.right() - 7.0 - cap_w, button.y + 6.5, cap_w, 18.0);
            c.round(cap, 4.0, palette::LIVE.mix(Rgb::BLACK, 0.24));
            c.text("Ctrl+F12", cap, &key);
        }
        (width, BAR_H, vec![(Part::End, button)])
    }

    /// The pill, likewise.
    fn pill(&self, p: &Painter, c: Option<&Canvas>) -> (f32, f32, Vec<(Part, Rect)>) {
        let (names, _) = self.who.clone().unwrap_or_default();
        let text = Text::new(13.0, Rgb::WHITE).middle();
        let (text_w, _) = p.measure(&names, &text, 2000.0);
        let text_x = 12.0 + 7.0 + 8.0;
        let close = Rect::new(text_x + text_w + 8.0, 4.0, 19.0, 19.0);
        let width = close.right() + 4.0;
        if let Some(c) = c {
            c.round_alpha(
                Rect::new(0.0, 0.0, width, PILL_H),
                PILL_H / 2.0,
                BACKGROUND,
                OPACITY,
            );
            c.circle(12.0 + 3.5, PILL_H / 2.0, 3.5, palette::TRAY_LIVE);
            c.text(&names, Rect::new(text_x, 0.0, text_w, PILL_H), &text);
            let fill = if self.hover == Some(Part::End) {
                CLOSE_HOVER
            } else {
                CLOSE
            };
            c.circle(close.x + 9.5, close.y + 9.5, 9.5, fill);
            c.icon(
                &icons::CLOSE,
                close.x + 4.0,
                close.y + 4.0,
                11.0,
                Rgb::WHITE,
            );
        }
        (
            width,
            PILL_H,
            vec![
                (Part::End, close),
                (Part::Pill, Rect::new(0.0, 0.0, close.x, PILL_H)),
            ],
        )
    }

    /// Draw the bar (or pill) and put it on screen, or hide it.
    fn render(&mut self) {
        if self.who.is_none() {
            // SAFETY: our own window.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            return;
        }
        // SAFETY: a plain query of our own window.
        self.dpi = match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 96,
            dpi => dpi,
        };
        let look = Look::with(true);
        let px = |dips: f32| (dips * self.dpi as f32 / 96.0).ceil() as i32;
        let shown = with_painter(|p| -> Result<Vec<(Part, Rect)>> {
            let layout = |c: Option<&Canvas>| {
                if self.expanded {
                    self.bar(p, c, &look)
                } else {
                    self.pill(p, c)
                }
            };
            let (w, h, parts) = layout(None);
            let surface = Surface::new(px(w), px(h))?;
            p.draw(&surface, self.dpi, |c| {
                layout(Some(c));
            })?;
            // SAFETY: a plain query.
            let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            // SAFETY: an out-parameter of the size it says.
            unsafe {
                let _ = GetMonitorInfoW(monitor, &mut info);
            }
            let work = info.rcWork;
            let top = if self.expanded { 0.0 } else { PILL_TOP };
            let at = POINT {
                x: work.left + (work.right - work.left - surface.width) / 2,
                y: work.top + px(top),
            };
            let size = SIZE {
                cx: surface.width,
                cy: surface.height,
            };
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
                SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )?;
            }
            Ok(parts)
        });
        match shown {
            Some(Ok(parts)) => self.parts = parts,
            Some(Err(e)) => debug!("showing the session bar: {e}"),
            None => {}
        }
    }

    fn part_at(&self, lparam: LPARAM) -> Option<Part> {
        let k = 96.0 / self.dpi.max(1) as f32;
        let at = (
            (lparam.0 & 0xFFFF) as i16 as f32 * k,
            ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 * k,
        );
        self.parts
            .iter()
            .find(|(_, rect)| rect.contains(at))
            .map(|(part, _)| *part)
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Never take the keyboard from what the user is doing.
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE | WM_MOUSELEAVE | WM_LBUTTONDOWN | WM_LBUTTONUP => {
            BAR.with(|bar| {
                // Not while the bar is being drawn or moved.
                let Ok(mut bar) = bar.try_borrow_mut() else {
                    return;
                };
                let Some(bar) = bar.as_mut() else { return };
                let before = (bar.hover, bar.pressed, bar.expanded);
                match msg {
                    WM_MOUSELEAVE => {
                        bar.tracking = false;
                        bar.hover = None;
                        bar.pressed = None;
                    }
                    WM_MOUSEMOVE => {
                        bar.hover = bar.part_at(lparam);
                        bar.active = Instant::now();
                        if !bar.tracking {
                            bar.tracking = true;
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
                    }
                    WM_LBUTTONDOWN => bar.pressed = bar.part_at(lparam),
                    _ => {
                        let part = bar.part_at(lparam);
                        if part.is_some() && part == bar.pressed.take() {
                            match part {
                                Some(Part::End) => bar.end = true,
                                _ => {
                                    bar.expanded = true;
                                    bar.active = Instant::now();
                                }
                            }
                        }
                    }
                }
                if before != (bar.hover, bar.pressed, bar.expanded) {
                    bar.render();
                }
            });
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub struct Indicator {
    hwnd: HWND,
}

impl Indicator {
    /// Create the (hidden) bar on the calling thread, which must run a
    /// message loop.
    pub fn new() -> Result<Self> {
        // SAFETY: registering a class and creating a window with static
        // strings and a valid window procedure, on this thread.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("RmmAgentSessionIndicator");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
                class,
                w!("Remote support session"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                warn!("the session bar will be visible to technicians too: {e}");
            }
            hwnd
        };
        let now = Instant::now();
        BAR.set(Some(Bar {
            hwnd,
            who: None,
            expanded: true,
            active: now,
            raised: now,
            hover: None,
            pressed: None,
            tracking: false,
            parts: Vec::new(),
            dpi: 96,
            end: false,
            stamp: look::stamp(),
        }));
        Ok(Self { hwnd })
    }

    /// Show the bar naming `who` (and whether that is several people),
    /// or hide it for `None`. A new arrival brings the full bar back.
    pub fn set(&mut self, who: Option<(String, bool)>) {
        BAR.with_borrow_mut(|bar| {
            let Some(bar) = bar.as_mut() else { return };
            if who == bar.who {
                return;
            }
            match &who {
                Some((names, _)) => info!(%names, "session bar shown"),
                None => info!("session bar hidden"),
            }
            bar.who = who;
            bar.expanded = true;
            bar.active = Instant::now();
            bar.hover = None;
            bar.pressed = None;
            bar.end = false;
            bar.render();
        });
    }

    /// Call every turn of the message loop: shrinks the bar to the pill
    /// when its time is up, follows a change of branding or scaling, and
    /// keeps it above other topmost windows.
    pub fn tick(&mut self) {
        BAR.with_borrow_mut(|bar| {
            let Some(bar) = bar.as_mut().filter(|bar| bar.who.is_some()) else {
                return;
            };
            let now = Instant::now();
            let mut redraw = false;
            if bar.expanded && bar.hover.is_none() && now - bar.active >= COLLAPSE_AFTER {
                bar.expanded = false;
                redraw = true;
            }
            let stamp = look::stamp();
            if stamp != bar.stamp {
                bar.stamp = stamp;
                redraw = true;
            }
            if now - bar.raised >= RAISE_EVERY {
                bar.raised = now;
                redraw = true;
            }
            if redraw {
                bar.render();
            }
        });
    }

    /// Shrink to the pill now, not when the bar's time is up (for
    /// `ui::preview`).
    pub fn collapse(&mut self) {
        BAR.with_borrow_mut(|bar| {
            if let Some(bar) = bar.as_mut() {
                bar.expanded = false;
                bar.render();
            }
        });
    }

    /// Whether the user pressed "End session" (or the pill's cross) since
    /// the last call.
    pub fn take_end(&mut self) -> bool {
        BAR.with_borrow_mut(|bar| bar.as_mut().is_some_and(|bar| std::mem::take(&mut bar.end)))
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        BAR.set(None);
        // SAFETY: our own window, on its thread.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
