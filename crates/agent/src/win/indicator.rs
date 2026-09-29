//! The on-screen session indicator: a small always-on-top banner at the top
//! of the primary screen for as long as a technician is connected, naming
//! them and reminding the user that Ctrl+F12 ends the session. The tray
//! icon is easy to miss; this is not. (Text: `interactive::indicator_text`.)
//!
//! It shows for `notify` and `require` sessions, the same technicians the
//! tray lists; `unattended` sessions are silent by policy.
//!
//! It must never get in the user's way, or become a way to interfere with
//! them:
//! - click-through (`WS_EX_LAYERED | WS_EX_TRANSPARENT`, `HTTRANSPARENT`)
//!   and never activated (`WS_EX_NOACTIVATE`, `MA_NOACTIVATE`), so neither
//!   the user's clicks nor the technician's injected ones land on it;
//! - excluded from screen capture (`WDA_EXCLUDEFROMCAPTURE`, Windows 10
//!   2004+), so it does not cover what the technician is looking at. On
//!   older Windows it simply appears in the stream too.
//!
//! It lives on the helper's UI thread, whose message loop (the tray's)
//! dispatches its messages. Other topmost windows can cover it, so the
//! tray loop re-asserts it every few seconds ([`Indicator::refresh`]).

use std::cell::RefCell;

use tracing::{debug, info, warn};
use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, GetDC, GetMonitorInfoW, MonitorFromPoint, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, SetWindowRgn, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DRAW_TEXT_FORMAT, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS,
    FW_SEMIBOLD, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, TRANSPARENT, VARIABLE_PITCH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA,
    MA_NOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WDA_EXCLUDEFROMCAPTURE,
    WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Background, dot and text colours (0x00BBGGRR).
const BACKGROUND: COLORREF = COLORREF(0x0024_2120);
const DOT: COLORREF = COLORREF(0x0036_43EA);
const TEXT: COLORREF = COLORREF(0x00FF_FFFF);
/// Slightly see-through.
const OPACITY: u8 = 235;

/// Sizes at 96 DPI; scaled for the monitor's DPI.
const FONT_PX: i32 = 15;
const PAD_X: i32 = 14;
const PAD_Y: i32 = 7;
const TOP_MARGIN: i32 = 8;
const CORNER: i32 = 12;

const DOT_TEXT: &str = "\u{25CF}  ";

thread_local! {
    /// What the window procedure paints (the window lives on this thread).
    static PAINT: RefCell<Option<Paint>> = const { RefCell::new(None) };
}

struct Paint {
    text: Vec<u16>,
    font: HFONT,
    pad_x: i32,
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Clicks go to whatever is underneath.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: WM_PAINT on our own window; GDI objects created here are
    // deleted here, and the font outlives the call (owned by `Indicator`).
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
        let brush = CreateSolidBrush(BACKGROUND);
        FillRect(hdc, &client, brush);
        let _ = DeleteObject(brush.into());
        PAINT.with_borrow(|paint| {
            if let Some(paint) = paint {
                draw(hdc, client, paint);
            }
        });
        let _ = EndPaint(hwnd, &ps);
    }
}

/// The red dot, then the text, vertically centred.
///
/// # Safety
/// `hdc` must be a valid device context for painting.
unsafe fn draw(hdc: HDC, client: RECT, paint: &Paint) {
    unsafe {
        let old = SelectObject(hdc, paint.font.into());
        SetBkMode(hdc, TRANSPARENT);
        let format: DRAW_TEXT_FORMAT = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        let mut dot = utf16(DOT_TEXT);
        let mut dot_rect = RECT::default();
        DrawTextW(hdc, &mut dot, &mut dot_rect, format | DT_CALCRECT);
        let mut rect = RECT {
            left: client.left + paint.pad_x,
            ..client
        };
        SetTextColor(hdc, DOT);
        DrawTextW(hdc, &mut dot, &mut rect, format);
        rect.left += dot_rect.right - dot_rect.left;
        SetTextColor(hdc, TEXT);
        let mut text = paint.text.clone();
        DrawTextW(hdc, &mut text, &mut rect, format);
        SelectObject(hdc, old);
    }
}

pub struct Indicator {
    hwnd: HWND,
    /// DPI the font was made for.
    dpi: u32,
    text: Option<String>,
}

impl Indicator {
    pub fn new() -> Result<Self> {
        // SAFETY: registering a class and creating a popup window with
        // static strings and a valid window procedure, on this thread.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("RmmAgentSessionIndicator");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // Fails harmlessly if already registered.
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
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
            SetLayeredWindowAttributes(hwnd, COLORREF(0), OPACITY, LWA_ALPHA)?;
            if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                warn!("session indicator will be visible to technicians too: {e}");
            }
            hwnd
        };
        Ok(Self {
            hwnd,
            dpi: 0,
            text: None,
        })
    }

    /// Show `text`, or hide the indicator for `None`.
    pub fn set(&mut self, text: Option<String>) {
        if text == self.text {
            return;
        }
        match &text {
            Some(t) => info!(text = %t, "session indicator shown"),
            None => info!("session indicator hidden"),
        }
        self.text = text;
        self.refresh();
    }

    /// Lay out and show (topmost again), or hide.
    pub fn refresh(&mut self) {
        let Some(text) = self.text.clone() else {
            // SAFETY: hiding our own window.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            return;
        };
        if let Err(e) = self.layout(&text) {
            debug!("laying out the session indicator: {e}");
        }
    }

    fn scale(&self, px: i32) -> i32 {
        px * self.dpi as i32 / 96
    }

    fn layout(&mut self, text: &str) -> Result<()> {
        // SAFETY: GDI and window calls on our own window, from its thread.
        // The previous font is deleted only after the new one is in place.
        unsafe {
            let dpi = match GetDpiForWindow(self.hwnd) {
                0 => 96,
                dpi => dpi,
            };
            let font = if dpi != self.dpi || PAINT.with_borrow(Option::is_none) {
                self.dpi = dpi;
                let font = CreateFontW(
                    -self.scale(FONT_PX),
                    0,
                    0,
                    0,
                    FW_SEMIBOLD.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    CLEARTYPE_QUALITY,
                    u32::from(VARIABLE_PITCH.0 | FF_SWISS.0),
                    w!("Segoe UI"),
                );
                if let Some(old) = PAINT.with_borrow(|p| p.as_ref().map(|p| p.font)) {
                    let _ = DeleteObject(old.into());
                }
                font
            } else {
                PAINT
                    .with_borrow(|p| p.as_ref().map(|p| p.font))
                    .unwrap_or_default()
            };

            // Measure the whole line.
            let mut line = utf16(&format!("{DOT_TEXT}{text}"));
            let hdc = GetDC(Some(self.hwnd));
            let old = SelectObject(hdc, font.into());
            let mut measured = RECT::default();
            DrawTextW(
                hdc,
                &mut line,
                &mut measured,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(Some(self.hwnd), hdc);

            let width = measured.right - measured.left + 2 * self.scale(PAD_X);
            let height = measured.bottom - measured.top + 2 * self.scale(PAD_Y);
            let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(monitor, &mut info);
            let work = info.rcWork;
            let x = work.left + (work.right - work.left - width) / 2;
            let y = work.top + self.scale(TOP_MARGIN);

            PAINT.set(Some(Paint {
                text: utf16(text),
                font,
                pad_x: self.scale(PAD_X),
            }));
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
            // Rounded corners. The system owns the region from here on.
            let corner = self.scale(CORNER);
            let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, corner, corner);
            SetWindowRgn(self.hwnd, Some(region), true);
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, true);
        }
        Ok(())
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        // SAFETY: destroying our own window on its thread, then its font.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            if let Some(paint) = PAINT.take() {
                let _ = DeleteObject(paint.font.into());
            }
        }
    }
}
