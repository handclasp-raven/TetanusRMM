//! A dialog as the artboards draw them: a native title bar, a white body
//! made of a few kinds of [`Block`], and a grey footer of buttons.
//!
//! The caller describes it with a [`Spec`] and may replace the spec at any
//! time (a countdown, a status line); the dialog lays it out, sizes its
//! window to fit, draws it and reports what the user did as [`Event`]s.
//! It has no message loop of its own: [`run`] is one for a thread that
//! shows a single dialog, and a program with more going on (quick assist)
//! pumps messages itself and calls [`Dialog::pre_translate`] on each.
//!
//! Keyboard: Tab and Shift+Tab move between the input and the buttons,
//! Enter and Space press the focused button, Enter in an input presses
//! the spec's default button, and Esc (and the close box) its cancel one.

use std::cell::RefCell;
use std::rc::Rc;

use brand::icons::{self, Icon};
use brand::Rgb;
use protocol::assist::{stressed, CODE_LEN};
use protocol::credential::MAX_PASSWORD_UNITS;
use protocol::ipc::Secret;
use tracing::{debug, info};
use windows::core::{w, Result, BOOL, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect,
    SetBkColor, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_SWISS,
    FW_NORMAL, HBRUSH, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, VARIABLE_PITCH,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_BACK, VK_CONTROL,
    VK_ESCAPE, VK_RETURN, VK_SHIFT, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetForegroundWindow, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextW,
    GetWindowThreadProcessId, KillTimer, LoadCursorW, MsgWaitForMultipleObjects, PeekMessageW,
    RegisterClassW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, ES_AUTOHSCROLL, ES_PASSWORD, GWLP_USERDATA,
    HWND_TOPMOST, ICON_BIG, ICON_SMALL, IDC_ARROW, MSG, PM_REMOVE, QS_ALLINPUT, SM_CXSCREEN,
    SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CHAR, WM_CLOSE, WM_CTLCOLOREDIT, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETFOCUS, WM_SETFONT, WM_SETICON, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_TOPMOST, WS_MINIMIZEBOX, WS_OVERLAPPED,
    WS_SYSMENU, WS_VISIBLE,
};
use zeroize::Zeroize;

use super::canvas::{with_painter, Align, Canvas, Painter, Rect, Surface, Text};
use super::look::{self, Look};

/// The dialog's width, and the margin either side of its contents.
pub const WIDTH: f32 = 484.0;
const PAD: f32 = 26.0;
/// The footer's height, and its buttons'.
const FOOTER: f32 = 72.0;
const BUTTON_H: f32 = 36.0;
const BUTTON_MIN_W: f32 = 115.0;
const BUTTON_GAP: f32 = 8.0;
/// The icon tile (or avatar) of a header.
const TILE: f32 = 46.0;
/// A digit's box in a code field.
const DIGIT_W: f32 = 46.0;
const DIGIT_H: f32 = 56.0;
const DIGIT_GAP: f32 = 9.0;
/// The wider gap in the middle of a code, where the dash is.
const DASH_GAP: f32 = 28.0;
const FIELD_H: f32 = 40.0;

/// Timer: watches for a change of look, and takes the keyboard once.
const TIMER: usize = 1;
const TIMER_MS: u32 = 200;

/// `EM_LIMITTEXT` and `EM_SETPASSWORDCHAR` (from winuser.h).
const EM_LIMITTEXT: u32 = 0x00C5;
const EM_SETPASSWORDCHAR: u32 = 0x00CC;
/// What stands for a typed character.
const BULLET: u16 = 0x25CF;
/// `WM_MOUSELEAVE` and `CF_UNICODETEXT`.
const WM_MOUSELEAVE: u32 = 0x02A3;
const CF_UNICODETEXT: u32 = 13;

/// What stands at the left of a header.
#[derive(Debug, Clone, PartialEq)]
pub enum Lead {
    /// An icon on its tile.
    Icon(Icon),
    /// A technician's initial in a circle.
    Avatar(String),
}

/// What colour a row's icon takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Accent,
    Live,
}

/// An icon and a line of text. `**` in the text marks words to stress.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub icon: Icon,
    pub tone: Tone,
    pub text: String,
}

/// A part of a dialog's body.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// The heading: what this is about, and a line under it.
    Header {
        lead: Lead,
        title: String,
        subtitle: String,
    },
    /// A muted line introducing what follows.
    Label(String),
    /// What something means, point by point.
    Rows(Vec<Row>),
    /// Assurances, each with a green check.
    Checks(Vec<String>),
    /// Points set apart on a panel.
    Panel(Vec<Row>),
    /// Time running out: a bar `fraction` (0-1) full, what happens at the
    /// end, and the reminder that Ctrl+F12 ends sessions.
    Countdown { fraction: f32, left: String },
    /// A password field under its label.
    Password { label: String },
    /// The boxes of a support code, under `label` and over `note`.
    Code {
        label: String,
        note: String,
        enabled: bool,
    },
    /// What is going on, or what went wrong.
    Status { text: String, error: bool },
    /// A paragraph.
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub id: u32,
    pub label: String,
    /// Filled with the accent colour: what the dialog is for.
    pub primary: bool,
    pub enabled: bool,
    /// At the footer's left edge instead of its right.
    pub left: bool,
}

impl Button {
    pub fn new(id: u32, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            primary: false,
            enabled: true,
            left: false,
        }
    }

    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn left(mut self) -> Self {
        self.left = true;
        self
    }
}

/// Where the keyboard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Button(u32),
    /// The password field or the code boxes.
    Input,
}

/// A dialog's contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub title: String,
    pub blocks: Vec<Block>,
    /// In the order they stand, left to right.
    pub buttons: Vec<Button>,
    /// Where the keyboard starts.
    pub focus: Focus,
    /// The button Enter presses from an input.
    pub default: Option<u32>,
    /// The button Esc and the close box press. `None`: Esc does nothing
    /// and the close box reports [`Event::Close`].
    pub cancel: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Above other windows, for a prompt that must be seen.
    pub topmost: bool,
    /// Take the keyboard when shown, though this is a background program.
    pub take_keyboard: bool,
    /// A minimise box, for a window that stays open.
    pub minimize: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Button(u32),
    /// The close box, with no cancel button to press.
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Button(u32),
    /// Show or hide what is typed in the password field.
    Eye,
    Code,
}

/// What laying out (and drawing) found.
#[derive(Default)]
struct Walk {
    height: f32,
    hits: Vec<(Hit, Rect)>,
    /// Where the password's edit control goes.
    edit: Option<Rect>,
}

struct State {
    hwnd: HWND,
    spec: Spec,
    options: Options,
    dpi: u32,
    surface: Option<Surface>,
    hits: Vec<(Hit, Rect)>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    focus: Focus,
    /// The keyboard has been used to move about: show where the focus is.
    keyboard: bool,
    events: Vec<Event>,
    /// The digits typed into a code block.
    code: String,
    edit: Option<HWND>,
    edit_font: HFONT,
    edit_brush: HBRUSH,
    edit_colors: Option<(Rgb, Rgb)>,
    reveal: bool,
    tracking: bool,
    /// The look last drawn with (see [`look::stamp`]).
    stamp: (u64, bool, bool),
    took_keyboard: bool,
}

fn colorref(c: Rgb) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF(
        u32::from(c.0) | u32::from(c.1) << 8 | u32::from(c.2) << 16,
    )
}

impl State {
    fn has_password(&self) -> bool {
        self.spec
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Password { .. }))
    }

    fn code_enabled(&self) -> bool {
        self.spec
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Code { enabled: true, .. }))
    }

    fn has_input(&self) -> bool {
        self.has_password() || self.code_enabled()
    }

    /// Where Tab goes, in order.
    fn order(&self) -> Vec<Focus> {
        let mut order = Vec::new();
        if self.has_input() {
            order.push(Focus::Input);
        }
        let enabled = |left: bool| {
            self.spec
                .buttons
                .iter()
                .filter(move |b| b.enabled && b.left == left)
                .map(|b| Focus::Button(b.id))
        };
        order.extend(enabled(true));
        order.extend(enabled(false));
        order
    }

    fn button(&self, id: u32) -> Option<&Button> {
        self.spec.buttons.iter().find(|b| b.id == id)
    }

    fn press(&mut self, id: u32) {
        if self.button(id).is_some_and(|b| b.enabled) {
            self.events.push(Event::Button(id));
        }
    }

    fn cancel(&mut self, close_box: bool) {
        match self.spec.cancel {
            Some(id) => self.press(id),
            None if close_box => self.events.push(Event::Close),
            None => {}
        }
    }

    /// Lay the dialog out in device-independent pixels, drawing it if
    /// there is a canvas to draw on.
    fn walk(&self, p: &Painter, c: Option<&Canvas>, look: &Look) -> Walk {
        let t = &look.theme;
        let mut out = Walk::default();
        let inner = WIDTH - 2.0 * PAD;
        let mut y = PAD;
        let body = Text::new(14.0, t.text);
        let muted = Text::new(14.0, t.text_muted);

        for block in &self.spec.blocks {
            match block {
                Block::Header {
                    lead,
                    title,
                    subtitle,
                } => {
                    let tile = Rect::new(PAD, y, TILE, TILE);
                    let x = PAD + TILE + 15.0;
                    let w = WIDTH - x - PAD;
                    let title_style = Text::new(20.0, t.text).semibold().wrap();
                    let sub_style = muted.wrap();
                    let (_, th) = p.measure(title, &title_style, w);
                    let (_, sh) = p.measure(subtitle, &sub_style, w);
                    let block_h = th + 1.0 + sh;
                    let top = y + ((TILE - block_h) / 2.0).max(0.0);
                    if let Some(c) = c {
                        match lead {
                            Lead::Icon(icon) => {
                                c.round(tile, 9.0, t.tile);
                                c.icon(icon, tile.x + 12.5, tile.y + 12.5, 21.0, t.accent_text);
                            }
                            Lead::Avatar(initial) => {
                                c.circle(
                                    tile.x + TILE / 2.0,
                                    tile.y + TILE / 2.0,
                                    TILE / 2.0,
                                    t.avatar,
                                );
                                c.text(
                                    initial,
                                    tile,
                                    &Text::new(18.0, t.on_avatar).semibold().centered(),
                                );
                            }
                        }
                        c.text(title, Rect::new(x, top, w, th), &title_style);
                        c.text(subtitle, Rect::new(x, top + th + 1.0, w, sh), &sub_style);
                    }
                    y += TILE.max(block_h) + 18.0;
                }
                Block::Label(text) => {
                    let style = muted.wrap();
                    let (_, h) = p.measure(text, &style, inner);
                    if let Some(c) = c {
                        c.text(text, Rect::new(PAD, y, inner, h), &style);
                    }
                    y += h + 10.0;
                }
                Block::Rows(rows) => {
                    let style = Text::new(15.0, t.text).wrap();
                    let x = PAD + 29.0;
                    for row in rows {
                        let (_, h) = p.measure(&row.text, &style, WIDTH - x - PAD);
                        if let Some(c) = c {
                            c.icon(&row.icon, PAD, y + 1.5, 17.0, tone(t, row.tone));
                            c.text(&row.text, Rect::new(x, y, WIDTH - x - PAD, h), &style);
                        }
                        y += h + 9.5;
                    }
                    y += 7.0;
                }
                Block::Checks(lines) => {
                    let style = body.wrap();
                    let x = PAD + 25.0;
                    for line in lines {
                        let (_, h) = p.measure(line, &style, WIDTH - x - PAD);
                        if let Some(c) = c {
                            c.icon(&icons::CHECK, PAD + 1.0, y + 2.5, 14.0, t.ok);
                            c.text(line, Rect::new(x, y, WIDTH - x - PAD, h), &style);
                        }
                        y += h + 6.0;
                    }
                    y += 12.0;
                }
                Block::Panel(rows) => {
                    let style = Text::new(14.5, t.text).wrap();
                    let x = PAD + 46.0;
                    let w = WIDTH - PAD - 18.0 - x;
                    let heights: Vec<f32> = rows
                        .iter()
                        .map(|row| p.measure_runs(&stressed(&row.text), &style, w).1)
                        .collect();
                    let gaps = 13.0 * rows.len().saturating_sub(1) as f32;
                    let height = 36.0 + heights.iter().sum::<f32>() + gaps;
                    if let Some(c) = c {
                        c.round(Rect::new(PAD, y, inner, height), 8.0, t.panel);
                        let mut row_y = y + 18.0;
                        for (row, h) in rows.iter().zip(&heights) {
                            c.icon(&row.icon, PAD + 18.0, row_y + 2.0, 16.0, tone(t, row.tone));
                            c.runs(&stressed(&row.text), Rect::new(x, row_y, w, *h), &style);
                            row_y += h + 13.0;
                        }
                    }
                    y += height + 22.0;
                }
                Block::Countdown { fraction, left } => {
                    let small = Text::new(13.5, t.text_muted).middle();
                    if let Some(c) = c {
                        c.round(Rect::new(PAD, y, inner, 4.0), 2.0, t.track);
                        let filled = inner * fraction.clamp(0.0, 1.0);
                        if filled >= 4.0 {
                            c.round(Rect::new(PAD, y, filled, 4.0), 2.0, t.accent_text);
                        }
                        let line = Rect::new(PAD, y + 13.0, inner, 20.0);
                        c.text(left, line, &small);
                        // From the right: F12, +, Ctrl, and what they do.
                        let mut right = line.right();
                        for (i, key) in ["F12", "Ctrl"].into_iter().enumerate() {
                            right = key_cap(c, p, look, key, right, line);
                            if i == 0 {
                                let (w, _) = p.measure("+", &small, 100.0);
                                c.text("+", Rect::new(right - w - 5.0, line.y, w, line.h), &small);
                                right -= w + 10.0;
                            }
                        }
                        let label = "End any time:";
                        let (w, _) = p.measure(label, &small, 300.0);
                        c.text(label, Rect::new(right - w - 7.0, line.y, w, line.h), &small);
                    }
                    y += 4.0 + 9.0 + 20.0 + 21.0;
                }
                Block::Password { label } => {
                    let style = body.semibold();
                    let (_, h) = p.measure(label, &style, inner);
                    let field = Rect::new(PAD, y + h + 8.0, inner, FIELD_H);
                    let eye = Rect::new(field.right() - 40.0, field.y, 40.0, FIELD_H);
                    if let Some(c) = c {
                        c.text(label, Rect::new(PAD, y, inner, h), &style);
                        c.round(field, 5.0, t.control);
                        c.outline(field, 5.0, t.control_line, 1.0);
                        // The field has the keyboard: underlined in the accent.
                        c.clipped(
                            Rect::new(field.x, field.bottom() - 2.0, field.w, 2.0),
                            || {
                                c.round(field, 5.0, t.accent_text);
                            },
                        );
                        let icon = if self.reveal {
                            &icons::VIEW_OFF
                        } else {
                            &icons::VIEW
                        };
                        let shade = if self.hover == Some(Hit::Eye) {
                            t.text
                        } else {
                            t.text_muted
                        };
                        c.icon(icon, eye.x + 11.0, eye.y + 11.0, 18.0, shade);
                    }
                    out.edit = Some(Rect::new(
                        field.x + 12.0,
                        field.y + 9.0,
                        field.w - 12.0 - 44.0,
                        22.0,
                    ));
                    out.hits.push((Hit::Eye, eye));
                    y = field.bottom() + 22.0;
                }
                Block::Code {
                    label,
                    note,
                    enabled,
                } => {
                    let label_style = muted.align(Align::Center);
                    let (_, lh) = p.measure(label, &label_style, inner);
                    let boxes_y = y + lh + 10.0;
                    let total =
                        CODE_LEN as f32 * DIGIT_W + (CODE_LEN - 2) as f32 * DIGIT_GAP + DASH_GAP;
                    let start = (WIDTH - total) / 2.0;
                    let half = CODE_LEN / 2;
                    let digits: Vec<char> = self.code.chars().collect();
                    let active = self.focus == Focus::Input && *enabled;
                    if let Some(c) = c {
                        c.text(label, Rect::new(PAD, y, inner, lh), &label_style);
                        let mut x = start;
                        for i in 0..CODE_LEN {
                            if i == half {
                                let mid = x - DIGIT_GAP + DASH_GAP / 2.0;
                                let at = boxes_y + DIGIT_H / 2.0;
                                c.line((mid - 5.0, at), (mid + 5.0, at), t.control_line, 2.0);
                                x += DASH_GAP - DIGIT_GAP;
                            }
                            let cell = Rect::new(x, boxes_y, DIGIT_W, DIGIT_H);
                            let current = active && i == digits.len().min(CODE_LEN - 1);
                            c.round(cell, 5.0, if *enabled { t.control } else { t.panel });
                            if current {
                                c.outline_alpha(cell.inflate(3.0), 8.0, t.accent_text, 3.0, 0.22);
                                c.outline(cell, 5.0, t.accent_text, 2.0);
                            } else {
                                c.outline(cell, 5.0, t.control_line, 1.0);
                                // A heavier foot, as Windows' own fields have.
                                c.clipped(
                                    Rect::new(cell.x, cell.bottom() - 2.0, cell.w, 2.0),
                                    || c.round(cell, 5.0, t.text_muted),
                                );
                            }
                            if let Some(digit) = digits.get(i) {
                                let shade = if *enabled { t.text } else { t.text_muted };
                                c.text(
                                    &digit.to_string(),
                                    cell,
                                    &Text::new(24.0, shade).semibold().centered(),
                                );
                            }
                            x += DIGIT_W + DIGIT_GAP;
                        }
                    }
                    out.hits
                        .push((Hit::Code, Rect::new(start, boxes_y, total, DIGIT_H)));
                    y = boxes_y + DIGIT_H + 22.0;
                    let note_style = muted.middle();
                    let (nw, _) = p.measure(note, &note_style, inner);
                    let x = (WIDTH - (nw + 22.0)) / 2.0;
                    if let Some(c) = c {
                        c.icon(&icons::LOCK, x, y + 2.0, 15.0, t.text_muted);
                        c.text(note, Rect::new(x + 22.0, y, nw, 20.0), &note_style);
                    }
                    y += 20.0 + 22.0;
                }
                Block::Status { text, error } => {
                    let shade = if *error { t.live_text } else { t.text_muted };
                    let style = Text::new(14.0, shade).align(Align::Center).wrap();
                    // Room for two lines whatever it says, so the window
                    // does not jump about as the status changes.
                    let (_, h) = p.measure(text, &style, inner);
                    let h = h.max(38.0);
                    if let Some(c) = c {
                        c.text(text, Rect::new(PAD, y - 8.0, inner, h), &style);
                    }
                    y += h + 6.0;
                }
                Block::Text(text) => {
                    let style = body.wrap();
                    let (_, h) = p.measure(text, &style, inner);
                    if let Some(c) = c {
                        c.text(text, Rect::new(PAD, y, inner, h), &style);
                    }
                    y += h + 16.0;
                }
            }
        }

        // The footer: buttons at the left edge, and at the right.
        let footer = Rect::new(0.0, y, WIDTH, FOOTER);
        if let Some(c) = c {
            c.rect(footer, t.footer);
            c.rect(Rect::new(0.0, y, WIDTH, 1.0), t.line);
        }
        let by = y + (FOOTER - BUTTON_H) / 2.0;
        let width = |b: &Button| {
            let style = Text::new(14.5, t.text).semibold();
            (p.measure(&b.label, &style, 400.0).0 + 36.0).max(BUTTON_MIN_W)
        };
        let mut left = PAD;
        for b in self.spec.buttons.iter().filter(|b| b.left) {
            let rect = Rect::new(left, by, width(b), BUTTON_H);
            left = rect.right() + BUTTON_GAP;
            self.draw_button(c, look, b, rect, &mut out);
        }
        let mut right = WIDTH - PAD;
        for b in self.spec.buttons.iter().filter(|b| !b.left).rev() {
            let rect = Rect::new(right - width(b), by, width(b), BUTTON_H);
            right = rect.x - BUTTON_GAP;
            self.draw_button(c, look, b, rect, &mut out);
        }
        out.height = y + FOOTER;
        out
    }

    fn draw_button(&self, c: Option<&Canvas>, look: &Look, b: &Button, rect: Rect, out: &mut Walk) {
        let t = &look.theme;
        if b.enabled {
            out.hits.push((Hit::Button(b.id), rect));
        }
        let Some(c) = c else { return };
        let hit = Some(Hit::Button(b.id));
        let (hover, pressed) = (self.hover == hit, self.pressed == hit && self.hover == hit);
        let label = Text::new(14.5, t.text).centered();
        if !b.enabled {
            c.round(rect, 5.0, t.track);
            c.text(
                &b.label,
                rect,
                &Text {
                    color: t.text_muted,
                    ..label
                },
            );
        } else if b.primary {
            let fill = if pressed {
                t.accent_pressed
            } else if hover {
                t.accent_hover
            } else {
                t.accent
            };
            c.round(rect, 5.0, fill);
            c.text(
                &b.label,
                rect,
                &Text {
                    color: t.on_accent,
                    ..label
                }
                .semibold(),
            );
        } else {
            c.round(rect, 5.0, if hover { t.control_hover } else { t.control });
            c.outline(rect, 5.0, t.control_line, 1.0);
            c.text(&b.label, rect, &label);
        }
        if self.keyboard && self.focus == Focus::Button(b.id) && b.enabled {
            c.outline(rect.inflate(3.0), 8.0, t.accent_text, 2.0);
        }
    }

    fn hit(&self, at: (f32, f32)) -> Option<Hit> {
        self.hits
            .iter()
            .find(|(_, rect)| rect.contains(at))
            .map(|(hit, _)| *hit)
    }

    fn invalidate(&self) {
        // SAFETY: our own window.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }
}

fn tone(t: &brand::Theme, tone: Tone) -> Rgb {
    match tone {
        Tone::Accent => t.accent_text,
        Tone::Live => t.live_text,
    }
}

/// A key's cap, its right edge at `right`, centred on `line`. Returns its
/// left edge.
fn key_cap(c: &Canvas, p: &Painter, look: &Look, key: &str, right: f32, line: Rect) -> f32 {
    let t = &look.theme;
    let style = Text::new(12.5, t.text).centered();
    let w = p.measure(key, &style, 100.0).0 + 13.0;
    let cap = Rect::new(right - w, line.y, w, 20.0);
    // The cap stands on a darker foot.
    c.round(
        Rect::new(cap.x, cap.y + 1.5, cap.w, cap.h),
        4.5,
        t.control_line,
    );
    c.round(cap, 4.0, t.control);
    c.outline(cap, 4.0, t.control_line, 1.0);
    c.text(key, cap, &style);
    cap.x
}

/// The text on the clipboard, if it holds any.
fn clipboard_text() -> Option<String> {
    // SAFETY: the clipboard is opened and closed here; the locked memory
    // is a NUL-terminated UTF-16 string, read up to its terminator.
    unsafe {
        OpenClipboard(None).ok()?;
        let text = GetClipboardData(CF_UNICODETEXT).ok().and_then(|handle| {
            let global = windows::Win32::Foundation::HGLOBAL(handle.0);
            let data = GlobalLock(global).cast::<u16>();
            if data.is_null() {
                return None;
            }
            let len = (0..4096).take_while(|&i| *data.add(i) != 0).count();
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(data, len));
            let _ = GlobalUnlock(global);
            Some(text)
        });
        let _ = CloseClipboard();
        text
    }
}

/// Make `hwnd` the foreground window, with the keyboard in `focus`.
/// Windows only lets the foreground application's threads do that, and
/// the helper is a background process: so for the moment of the call,
/// this thread shares the input state of the thread that owns the
/// foreground window.
fn take_keyboard(hwnd: HWND, focus: HWND) {
    // SAFETY: plain window and thread queries; the attachment is undone
    // before returning.
    unsafe {
        let me = GetCurrentThreadId();
        let foreground = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = foreground != 0
            && foreground != me
            && AttachThreadInput(me, foreground, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let taken = SetForegroundWindow(hwnd).as_bool();
        let _ = SetFocus(Some(focus));
        if attached {
            let _ = AttachThreadInput(me, foreground, false);
        }
        info!(taken, attached, "dialog on screen");
    }
}

fn state_of<'a>(hwnd: HWND) -> Option<&'a RefCell<State>> {
    // SAFETY: the pointer is the one `Dialog::open` stored, to a cell
    // that lives until after the window is destroyed.
    unsafe {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        ptr.as_ref()
    }
}

fn mouse_at(state: &State, lparam: LPARAM) -> (f32, f32) {
    let (x, y) = (
        (lparam.0 & 0xFFFF) as i16 as f32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as f32,
    );
    let k = 96.0 / state.dpi as f32;
    (x * k, y * k)
}

fn paint(cell: &RefCell<State>, hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    // SAFETY: a paint of our own window, begun and ended here; the blit is
    // between a surface of ours and the paint's device context.
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        if let Ok(mut state) = cell.try_borrow_mut() {
            let look = Look::current();
            let mut client = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client);
            let (w, h) = (client.right - client.left, client.bottom - client.top);
            if state
                .surface
                .as_ref()
                .is_none_or(|s| s.width != w || s.height != h)
            {
                state.surface = Surface::new(w, h).ok();
            }
            let dpi = state.dpi;
            let hits = state.surface.as_ref().and_then(|surface| {
                with_painter(|p| {
                    let mut hits = Vec::new();
                    let drawn = p.draw(surface, dpi, |c| {
                        c.clear(look.theme.body);
                        hits = state.walk(p, Some(c), &look).hits;
                    });
                    if let Err(e) = drawn {
                        debug!("drawing a dialog: {e}");
                    }
                    let _ = BitBlt(hdc, 0, 0, w, h, Some(surface.hdc()), 0, 0, SRCCOPY);
                    hits
                })
            });
            if let Some(hits) = hits {
                state.hits = hits;
            }
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(cell) = state_of(hwnd) else {
        // SAFETY: default handling, before the dialog's state is attached.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_PAINT => {
            paint(cell, hwnd);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_CLOSE => {
            if let Ok(mut state) = cell.try_borrow_mut() {
                state.cancel(true);
            }
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_TIMER => {
            let mut retheme = false;
            let mut keyboard = None;
            if let Ok(mut state) = cell.try_borrow_mut() {
                let stamp = look::stamp();
                if stamp != state.stamp {
                    state.stamp = stamp;
                    retheme = true;
                }
                if msg == WM_TIMER && state.options.take_keyboard && !state.took_keyboard {
                    state.took_keyboard = true;
                    keyboard = Some(match (state.focus, state.edit) {
                        (Focus::Input, Some(edit)) => edit,
                        _ => hwnd,
                    });
                }
            }
            if retheme {
                apply_look(cell, hwnd);
            }
            if let Some(focus) = keyboard {
                take_keyboard(hwnd, focus);
            }
            return LRESULT(0);
        }
        WM_DPICHANGED => {
            if let Ok(mut state) = cell.try_borrow_mut() {
                state.dpi = (wparam.0 & 0xFFFF) as u32;
            }
            apply_look(cell, hwnd);
            relayout(cell, hwnd, false);
            return LRESULT(0);
        }
        WM_CTLCOLOREDIT => {
            if let Ok(state) = cell.try_borrow() {
                if let Some((text, back)) = state.edit_colors {
                    let hdc = HDC(wparam.0 as *mut _);
                    // SAFETY: the device context Windows passed for our own
                    // edit control.
                    unsafe {
                        SetTextColor(hdc, colorref(text));
                        SetBkColor(hdc, colorref(back));
                    }
                    return LRESULT(state.edit_brush.0 as isize);
                }
            }
        }
        WM_SETFOCUS => {
            // The keyboard belongs in the password field, if that is where
            // the focus is.
            let edit = cell
                .try_borrow()
                .ok()
                .and_then(|s| (s.focus == Focus::Input).then_some(s.edit).flatten());
            if let Some(edit) = edit {
                // SAFETY: our own child window.
                unsafe {
                    let _ = SetFocus(Some(edit));
                }
            }
            return LRESULT(0);
        }
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            if let Ok(mut state) = cell.try_borrow_mut() {
                let hover = (msg == WM_MOUSEMOVE)
                    .then(|| state.hit(mouse_at(&state, lparam)))
                    .flatten();
                if msg == WM_MOUSELEAVE {
                    state.tracking = false;
                } else if !state.tracking {
                    state.tracking = true;
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
                if hover != state.hover {
                    state.hover = hover;
                    state.invalidate();
                }
            }
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            let mut refocus = false;
            let mut toggle_reveal = None;
            if let Ok(mut state) = cell.try_borrow_mut() {
                let hit = state.hit(mouse_at(&state, lparam));
                if msg == WM_LBUTTONDOWN {
                    state.pressed = hit;
                    state.keyboard = false;
                    if hit == Some(Hit::Code) && state.code_enabled() {
                        state.focus = Focus::Input;
                        refocus = true;
                    }
                } else {
                    let pressed = state.pressed.take();
                    if pressed.is_some() && pressed == hit {
                        match pressed {
                            Some(Hit::Button(id)) => state.press(id),
                            Some(Hit::Eye) => {
                                state.reveal = !state.reveal;
                                toggle_reveal = state.edit.map(|edit| (edit, state.reveal));
                            }
                            _ => {}
                        }
                    }
                }
                state.invalidate();
            }
            if let Some((edit, reveal)) = toggle_reveal {
                let mask = if reveal { 0 } else { usize::from(BULLET) };
                // SAFETY: messages to our own edit control.
                unsafe {
                    SendMessageW(
                        edit,
                        EM_SETPASSWORDCHAR,
                        Some(WPARAM(mask)),
                        Some(LPARAM(0)),
                    );
                    let _ = InvalidateRect(Some(edit), None, true);
                    let _ = SetFocus(Some(edit));
                }
            }
            if refocus {
                // SAFETY: our own window.
                unsafe {
                    let _ = SetFocus(Some(hwnd));
                }
            }
            return LRESULT(0);
        }
        WM_KEYDOWN => {
            key_down(cell, hwnd, wparam.0 as u16);
            return LRESULT(0);
        }
        WM_CHAR => {
            if let Ok(mut state) = cell.try_borrow_mut() {
                if state.code_enabled() {
                    let typed = char::from_u32(wparam.0 as u32).unwrap_or_default();
                    let add: String = if typed == '\u{16}' {
                        // Ctrl+V: the digits of whatever was copied.
                        clipboard_text().unwrap_or_default()
                    } else {
                        typed.to_string()
                    };
                    let before = state.code.len();
                    for digit in add.chars().filter(char::is_ascii_digit) {
                        if state.code.len() < CODE_LEN {
                            state.code.push(digit);
                        }
                    }
                    if state.code.len() != before {
                        state.focus = Focus::Input;
                        state.invalidate();
                    }
                }
            }
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A key pressed in the dialog (or passed on from its edit control).
fn key_down(cell: &RefCell<State>, hwnd: HWND, key: u16) {
    let mut focus_window = None;
    if let Ok(mut state) = cell.try_borrow_mut() {
        // SAFETY: plain key state queries.
        let (shift, control) = unsafe {
            (
                GetKeyState(i32::from(VK_SHIFT.0)) < 0,
                GetKeyState(i32::from(VK_CONTROL.0)) < 0,
            )
        };
        match key {
            k if k == VK_TAB.0 && !control => {
                let order = state.order();
                if !order.is_empty() {
                    let at = order.iter().position(|f| *f == state.focus);
                    let next = match (at, shift) {
                        (Some(i), false) => (i + 1) % order.len(),
                        (Some(i), true) => (i + order.len() - 1) % order.len(),
                        (None, _) => 0,
                    };
                    state.focus = order[next];
                    state.keyboard = true;
                    focus_window = Some(match (state.focus, state.edit) {
                        (Focus::Input, Some(edit)) => edit,
                        _ => hwnd,
                    });
                }
            }
            k if k == VK_RETURN.0 => match state.focus {
                Focus::Button(id) => state.press(id),
                Focus::Input => {
                    if let Some(id) = state.spec.default {
                        state.press(id);
                    }
                }
            },
            k if k == VK_SPACE.0 => {
                if let Focus::Button(id) = state.focus {
                    state.press(id);
                }
            }
            k if k == VK_ESCAPE.0 => state.cancel(false),
            k if k == VK_BACK.0 && state.code_enabled() => {
                state.code.pop();
                state.focus = Focus::Input;
            }
            _ => {}
        }
        state.invalidate();
    }
    if let Some(window) = focus_window {
        // SAFETY: our own window or its child.
        unsafe {
            let _ = SetFocus(Some(window));
        }
    }
}

/// Take on the current look: the title bar's shade, the icon, and the
/// password field's colours.
fn apply_look(cell: &RefCell<State>, hwnd: HWND) {
    let look = Look::current();
    let dark = BOOL::from(look.theme.dark);
    // SAFETY: a BOOL of the size given, for our own window.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&raw const dark).cast(),
            std::mem::size_of::<BOOL>() as u32,
        );
    }
    let Ok(mut state) = cell.try_borrow_mut() else {
        return;
    };
    let dpi = state.dpi as i32;
    for (which, size) in [(ICON_SMALL, 16), (ICON_BIG, 32)] {
        if let Some(icon) = look::window_icon(size * dpi / 96) {
            // SAFETY: an icon handle that outlives the window's use of it.
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(which as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
            }
        }
    }
    let colors = (look.theme.text, look.theme.control);
    if state.edit_colors != Some(colors) {
        state.edit_colors = Some(colors);
        // SAFETY: the old brush is ours and no longer selected anywhere.
        unsafe {
            if !state.edit_brush.is_invalid() {
                let _ = DeleteObject(state.edit_brush.into());
            }
            state.edit_brush = CreateSolidBrush(colorref(colors.1));
        }
    }
    if let Some(edit) = state.edit {
        // SAFETY: fonts and messages of our own.
        unsafe {
            if !state.edit_font.is_invalid() {
                let _ = DeleteObject(state.edit_font.into());
            }
            state.edit_font = CreateFontW(
                -(15 * dpi / 96),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
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
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(state.edit_font.0 as usize)),
                Some(LPARAM(1)),
            );
            let _ = InvalidateRect(Some(edit), None, true);
        }
    }
    state.invalidate();
}

fn window_style(options: &Options) -> WINDOW_STYLE {
    let mut style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    if options.minimize {
        style |= WS_MINIMIZEBOX;
    }
    style
}

/// Size the window to its contents, and put the edit control in its place.
/// `center`: also centre it on the screen (when first shown).
fn relayout(cell: &RefCell<State>, hwnd: HWND, center: bool) {
    let Some((walk, dpi, options, edit)) = cell.try_borrow().ok().and_then(|state| {
        let look = Look::current();
        with_painter(|p| state.walk(p, None, &look))
            .map(|walk| (walk, state.dpi, state.options, state.edit))
    }) else {
        return;
    };
    let px = |dips: f32| (dips * dpi as f32 / 96.0).round() as i32;
    let ex_style = if options.topmost {
        WS_EX_TOPMOST
    } else {
        WINDOW_EX_STYLE(0)
    };
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: px(WIDTH),
        bottom: px(walk.height),
    };
    // SAFETY: sizing and placing our own windows.
    unsafe {
        let _ = AdjustWindowRectExForDpi(&mut frame, window_style(&options), false, ex_style, dpi);
        let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
        let (x, y) = (
            (GetSystemMetrics(SM_CXSCREEN) - w) / 2,
            (GetSystemMetrics(SM_CYSCREEN) - h) / 2,
        );
        let order = options.topmost.then_some(HWND_TOPMOST);
        let mut flags = SWP_NOACTIVATE;
        if !center {
            flags |= SWP_NOMOVE;
        }
        if order.is_none() {
            flags |= SWP_NOZORDER;
        }
        let _ = SetWindowPos(hwnd, order, x, y.max(0), w, h, flags);
        if let (Some(edit), Some(r)) = (edit, walk.edit) {
            let _ = SetWindowPos(
                edit,
                None,
                px(r.x),
                px(r.y),
                px(r.w),
                px(r.h),
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// A dialog on screen. Dropping it closes it.
pub struct Dialog {
    hwnd: HWND,
    state: Rc<RefCell<State>>,
}

impl Dialog {
    /// Show a dialog, centred on the screen.
    pub fn open(spec: Spec, options: Options) -> Result<Self> {
        let ex_style = if options.topmost {
            WS_EX_TOPMOST
        } else {
            WINDOW_EX_STYLE(0)
        };
        // SAFETY: registering a class and creating windows with static or
        // owned strings and a valid window procedure, on this thread.
        let (hwnd, edit) = unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("TetanusRmmDialog");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                ex_style,
                class,
                &HSTRING::from(spec.title.as_str()),
                window_style(&options),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            let edit = if spec
                .blocks
                .iter()
                .any(|b| matches!(b, Block::Password { .. }))
            {
                let edit = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("EDIT"),
                    w!(""),
                    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | (ES_PASSWORD | ES_AUTOHSCROLL) as u32),
                    0,
                    0,
                    0,
                    0,
                    Some(hwnd),
                    None,
                    Some(instance.into()),
                    None,
                )?;
                SendMessageW(
                    edit,
                    EM_LIMITTEXT,
                    Some(WPARAM(MAX_PASSWORD_UNITS)),
                    Some(LPARAM(0)),
                );
                SendMessageW(
                    edit,
                    EM_SETPASSWORDCHAR,
                    Some(WPARAM(usize::from(BULLET))),
                    Some(LPARAM(0)),
                );
                Some(edit)
            } else {
                None
            };
            (hwnd, edit)
        };
        // SAFETY: a plain query of our own window.
        let dpi = match unsafe { GetDpiForWindow(hwnd) } {
            0 => 96,
            dpi => dpi,
        };
        let focus = spec.focus;
        let state = Rc::new(RefCell::new(State {
            hwnd,
            spec,
            options,
            dpi,
            surface: None,
            hits: Vec::new(),
            hover: None,
            pressed: None,
            focus,
            keyboard: false,
            events: Vec::new(),
            code: String::new(),
            edit,
            edit_font: HFONT::default(),
            edit_brush: HBRUSH::default(),
            edit_colors: None,
            reveal: false,
            tracking: false,
            stamp: look::stamp(),
            took_keyboard: false,
        }));
        // SAFETY: the pointer stays valid until `drop`, which destroys the
        // window before releasing the state.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Rc::as_ptr(&state) as isize);
        }
        apply_look(&state, hwnd);
        relayout(&state, hwnd, true);
        // SAFETY: showing our own window and starting its timer.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let target = match (focus, edit) {
                (Focus::Input, Some(edit)) => edit,
                _ => hwnd,
            };
            if !SetForegroundWindow(hwnd).as_bool() {
                let _ = BringWindowToTop(hwnd);
            }
            let _ = SetFocus(Some(target));
            SetTimer(Some(hwnd), TIMER, TIMER_MS, None);
        }
        Ok(Self { hwnd, state })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Replace the contents. The window is resized if they need it; the
    /// focus stays where it is if that is still possible.
    pub fn set(&self, spec: Spec) {
        let mut retitle = None;
        {
            let mut state = self.state.borrow_mut();
            if state.spec == spec {
                return;
            }
            if state.spec.title != spec.title {
                retitle = Some(spec.title.clone());
            }
            let page_changed = state.spec.blocks.len() != spec.blocks.len()
                || state.spec.buttons.len() != spec.buttons.len();
            state.spec = spec;
            if page_changed || !state.order().contains(&state.focus) {
                state.focus = state.spec.focus;
                if !state.order().contains(&state.focus) {
                    if let Some(first) = state.order().first() {
                        state.focus = *first;
                    }
                }
            }
        }
        if let Some(title) = retitle {
            // SAFETY: our own window, an owned string.
            unsafe {
                let _ = SetWindowTextW(self.hwnd, &HSTRING::from(title));
            }
        }
        relayout(&self.state, self.hwnd, false);
    }

    /// What the user did since the last call.
    pub fn events(&self) -> Vec<Event> {
        std::mem::take(&mut self.state.borrow_mut().events)
    }

    /// Give the dialog first refusal of a message from the queue: the
    /// keys that move about, typed into its password field. `true` if it
    /// was used up.
    pub fn pre_translate(&self, msg: &MSG) -> bool {
        let edit = self.state.borrow().edit;
        if edit != Some(msg.hwnd) {
            return false;
        }
        let key = msg.wParam.0 as u16;
        let moves = [VK_TAB.0, VK_RETURN.0, VK_ESCAPE.0].contains(&key);
        match msg.message {
            WM_KEYDOWN if moves => {
                key_down(&self.state, self.hwnd, key);
                true
            }
            // Their characters would only make the field beep.
            WM_CHAR if matches!(key, 0x09 | 0x0D | 0x1B) => true,
            _ => false,
        }
    }

    /// The digits typed into the code boxes.
    pub fn code(&self) -> String {
        self.state.borrow().code.clone()
    }

    /// Empty the code boxes, for another try.
    pub fn clear_code(&self) {
        let mut state = self.state.borrow_mut();
        state.code.clear();
        state.focus = Focus::Input;
        state.invalidate();
    }

    /// What is typed in the password field, if anything. The field is
    /// emptied, and the copy made on the way is wiped.
    pub fn take_password(&self) -> Option<Secret> {
        let edit = self.state.borrow().edit?;
        let mut buf = [0u16; MAX_PASSWORD_UNITS + 1];
        // SAFETY: reads our own control's text into `buf`, then empties it.
        let len = unsafe { GetWindowTextW(edit, &mut buf) } as usize;
        let secret = (len > 0).then(|| Secret::new(buf[..len.min(MAX_PASSWORD_UNITS)].to_vec()));
        buf.zeroize();
        if secret.is_some() {
            self.clear_password();
        }
        secret
    }

    /// Empty the password field.
    pub fn clear_password(&self) {
        if let Some(edit) = self.state.borrow().edit {
            // SAFETY: our own control.
            unsafe {
                let _ = SetWindowTextW(edit, w!(""));
            }
        }
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        self.clear_password();
        // SAFETY: our own window, timer and GDI objects; the state is
        // detached first so that late messages find nothing.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER);
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            let state = self.state.borrow();
            if !state.edit_font.is_invalid() {
                let _ = DeleteObject(state.edit_font.into());
            }
            if !state.edit_brush.is_invalid() {
                let _ = DeleteObject(state.edit_brush.into());
            }
        }
    }
}

/// A message loop for a thread whose only window is `dialog`. `step` is
/// called about ten times a second and after every burst of input, with
/// what the user did; the loop ends when it returns `false`.
pub fn run(dialog: &Dialog, mut step: impl FnMut(&Dialog, &[Event]) -> bool) {
    loop {
        // SAFETY: waits on, then drains, this thread's own message queue.
        unsafe {
            MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if !dialog.pre_translate(&msg) {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        if !step(dialog, &dialog.events()) {
            return;
        }
    }
}
