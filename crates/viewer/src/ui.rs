//! The viewer's controls around the remote picture: a toolbar along the
//! top (display mode, frame rate, monitor, text size, and the panel's
//! toggle) and a panel on the right (the session's buttons, the
//! technician's password vault, the lent password, the agent's status,
//! command buttons, file transfer), plus drop-down menus, a one-line text
//! prompt, a confirmation box and notices.
//!
//! Everything here is layout and drawing. A [`Chrome`] describes what is on
//! screen; the same geometry serves drawing ([`Chrome::draw`]) and clicks
//! ([`Chrome::hit`]), so the two cannot disagree. The window code in
//! `main.rs` owns the state and acts on the [`Action`]s.

use std::path::PathBuf;
use std::time::Instant;

use protocol::media::{FrameRate, MonitorInfo};
use protocol::vault::TextKind;

use crate::api::AgentDetails;
use crate::render::{self, Canvas, DisplayMode, Rect};

/// Colours (`0x00RRGGBB`).
pub mod colors {
    pub const TOOLBAR: u32 = 0x001f_2430;
    pub const PANEL: u32 = 0x001a_1e27;
    pub const BORDER: u32 = 0x0033_3a4a;
    pub const BUTTON: u32 = 0x002c_3342;
    pub const HOVER: u32 = 0x003a_4458;
    pub const ACTIVE: u32 = 0x0034_5a86;
    pub const TEXT: u32 = 0x00e6_e6e6;
    pub const MUTED: u32 = 0x0096_a0b0;
    pub const DISABLED: u32 = 0x005e_6676;
    pub const HEADING: u32 = 0x00ff_d27a;
    pub const BAR: u32 = 0x002c_3342;
    pub const OK: u32 = 0x004c_af50;
    pub const WARN: u32 = 0x00e0_a030;
    pub const FULL: u32 = 0x00e0_5050;
    pub const NOTICE: u32 = 0x0026_3446;
    pub const ERROR: u32 = 0x005c_2630;
    pub const FIELD: u32 = 0x0010_1318;
}

/// Characters of text across the side panel.
pub const PANEL_CHARS: u32 = 26;
/// Text size (glyph height in pixels at a scale factor of 1) unless the
/// technician picks another.
pub const DEFAULT_FONT_PX: u32 = 10;
/// Smallest and largest text size accepted.
pub const MIN_FONT_PX: u32 = 6;
pub const MAX_FONT_PX: u32 = 32;
/// The sizes the Text menu offers.
pub const TEXT_SIZES: [u32; 8] = [8, 9, 10, 11, 12, 14, 16, 20];
/// Widest a notice gets, in characters.
const NOTICE_CHARS: usize = 60;
/// Widest the prompt's field gets, in characters.
const PROMPT_CHARS: u32 = 48;
/// Items of the vault the panel shows at once.
pub const VAULT_ROWS: usize = 4;

/// Sizes in physical pixels, from the text size and the window's scale
/// factor. Padding and spacing follow the text size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    /// Glyph size: each character is `font` pixels square.
    pub font: u32,
    pub pad: u32,
    pub gap: u32,
}

impl Metrics {
    /// Text `font_px` high (clamped to [`MIN_FONT_PX`]..=[`MAX_FONT_PX`])
    /// at a scale factor of 1.
    pub fn new(font_px: u32, factor: f64) -> Self {
        let font = f64::from(font_px.clamp(MIN_FONT_PX, MAX_FONT_PX));
        let factor = factor.max(1.0);
        let px = |v: f64| ((v * factor).round() as u32).max(1);
        Self {
            font: px(font),
            pad: px(font / 3.0),
            gap: px(font / 4.0),
        }
    }

    /// The default text size.
    pub fn for_scale_factor(factor: f64) -> Self {
        Self::new(DEFAULT_FONT_PX, factor)
    }

    pub fn char_w(&self) -> u32 {
        self.font
    }

    pub fn glyph_h(&self) -> u32 {
        self.font
    }

    pub fn line_h(&self) -> u32 {
        self.glyph_h() + self.gap
    }

    /// Buttons and menu rows: the text with padding, and some extra room
    /// above and below so they are easy to hit.
    pub fn button_h(&self) -> u32 {
        self.glyph_h() + 2 * (self.pad + self.gap)
    }

    /// Where text goes in `rect` to sit centred vertically.
    pub fn text_y(&self, rect: Rect) -> u32 {
        rect.y + rect.h.saturating_sub(self.glyph_h()) / 2
    }

    pub fn toolbar_h(&self) -> u32 {
        self.button_h() + 2 * self.gap
    }

    pub fn panel_w(&self) -> u32 {
        PANEL_CHARS * self.char_w() + 2 * self.pad + 2 * self.gap
    }

    /// A disk usage bar's height.
    pub fn bar_h(&self) -> u32 {
        (self.font / 2).max(2)
    }

    pub fn border(&self) -> u32 {
        (self.font / 12).max(1)
    }
}

/// Where the toolbar, the panel and the remote picture go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub window: Rect,
    pub toolbar: Rect,
    /// `None` when hidden, or when the window is too narrow for it.
    pub panel: Option<Rect>,
    pub desktop: Rect,
}

pub fn layout(width: u32, height: u32, m: &Metrics, show_panel: bool) -> Layout {
    let window = Rect::new(0, 0, width, height);
    let toolbar_h = m.toolbar_h().min(height);
    let body_h = height - toolbar_h;
    // Leave the picture at least as much room as the panel takes.
    let panel_w = if show_panel && width >= 2 * m.panel_w() {
        m.panel_w()
    } else {
        0
    };
    Layout {
        window,
        toolbar: Rect::new(0, 0, width, toolbar_h),
        panel: (panel_w > 0).then(|| Rect::new(width - panel_w, toolbar_h, panel_w, body_h)),
        desktop: Rect::new(0, toolbar_h, width - panel_w, body_h),
    }
}

/// A button on the viewer's own controls: in the toolbar or the panel,
/// or an item of a menu or dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    DisplayMenu,
    FrameRateMenu,
    SetFrameRate(FrameRate),
    TextMenu,
    /// Text this many pixels high (at a scale factor of 1).
    SetTextSize(u32),
    MonitorMenu,
    SetDisplay(DisplayMode),
    SelectMonitor(u32),
    Keyframe,
    /// Ctrl+Alt+Del on the remote machine.
    SecureAttention,
    /// Ask the remote user to lend a password (see `protocol::credential`).
    PasswordRequest,
    /// Have the agent type the lent password on the remote machine.
    PasswordType,
    /// Have the agent forget it.
    PasswordForget,
    /// Ask for the master password, to unlock the vault (see [`Vault`]).
    VaultUnlock,
    /// Give the vault's search field the keyboard.
    VaultFocus,
    /// Search the vault for what is in its field.
    VaultSearch,
    /// Pick the item at this index of the results.
    VaultSelect(usize),
    /// Have this of the picked item typed on the remote machine.
    VaultType(TextKind),
    /// Fetch the latest vault from the Bitwarden server.
    VaultSync,
    VaultLock,
    ToggleFullscreen,
    TogglePanel,
    Disconnect,
    /// The command button at this index.
    Launch(usize),
    Upload,
    Download,
    PromptOk,
    PromptCancel,
    ConfirmOk,
    ConfirmCancel,
}

/// A command button: what it says, and what it starts on the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickCommand {
    pub label: String,
    pub command: String,
}

impl std::str::FromStr for QuickCommand {
    type Err = String;

    /// `Label=command`, e.g. `Network connections=ncpa.cpl`. A bare command
    /// is its own label.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (label, command) = match s.split_once('=') {
            Some((label, command)) => (label.trim(), command.trim()),
            None => (s.trim(), s.trim()),
        };
        if command.is_empty() {
            return Err(format!("no command in {s:?} (expected LABEL=COMMAND)"));
        }
        Ok(Self {
            label: if label.is_empty() { command } else { label }.to_owned(),
            command: command.to_owned(),
        })
    }
}

/// The built-in command buttons, when the TUI passes none.
pub fn default_commands() -> Vec<QuickCommand> {
    [
        ("Command prompt", "cmd"),
        ("Network connections", "ncpa.cpl"),
        ("Remote desktop", "mstsc"),
    ]
    .into_iter()
    .map(|(label, command)| QuickCommand {
        label: label.into(),
        command: command.into(),
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub rect: Rect,
    pub label: String,
    pub action: Action,
    pub enabled: bool,
    /// Shown pressed (e.g. the panel toggle while the panel is shown).
    pub active: bool,
    /// Opens a menu: drawn with a drop-down marker.
    pub dropdown: bool,
}

impl Button {
    fn new(rect: Rect, label: impl Into<String>, action: Action) -> Self {
        Self {
            rect,
            label: label.into(),
            action,
            enabled: true,
            active: false,
            dropdown: false,
        }
    }
}

/// A drop-down menu under a toolbar button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    /// The button it opened from.
    pub anchor: Rect,
    pub items: Vec<MenuItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub action: Action,
    /// The current choice.
    pub checked: bool,
}

/// What a text prompt is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptPurpose {
    /// Where on the agent to put this local file.
    Upload { local: PathBuf },
    /// Which file on the agent to download.
    Download,
    /// The master password, to unlock the vault. Never kept: see
    /// `crate::vault`.
    VaultUnlock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub title: String,
    pub text: String,
    pub purpose: PromptPurpose,
    /// Show `*` for each character typed (a password).
    pub masked: bool,
}

/// An item of the vault as the panel lists it. No secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRow {
    pub name: String,
    /// Its username, or its address when it has none.
    pub detail: String,
    /// What it has to type.
    pub username: bool,
    pub password: bool,
    pub totp: bool,
}

impl VaultRow {
    pub fn has(&self, kind: TextKind) -> bool {
        match kind {
            TextKind::Username => self.username,
            TextKind::Password => self.password,
            TextKind::Totp => self.totp,
        }
    }
}

/// The vault section of the panel: locked, an unlock button; unlocked, a
/// search field, the items found, and what to type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vault {
    /// Whether the vault can be searched (a session key opened it).
    pub unlocked: bool,
    /// Whether keys go to the search field rather than the remote machine.
    pub focused: bool,
    pub query: String,
    pub rows: Vec<VaultRow>,
    pub selected: usize,
    /// The first row shown.
    pub top: usize,
    /// What is going on ("Searching..."); nothing can be done meanwhile.
    pub busy: Option<String>,
    /// What came of the last thing done, and whether it went wrong.
    pub note: Option<(String, bool)>,
}

impl Vault {
    /// The picked item, if there are any.
    pub fn current(&self) -> Option<&VaultRow> {
        self.rows.get(self.selected)
    }

    /// Pick `index` (clamped), scrolling it into view.
    pub fn select(&mut self, index: usize) {
        self.selected = index.min(self.rows.len().saturating_sub(1));
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + VAULT_ROWS {
            self.top = self.selected + 1 - VAULT_ROWS;
        }
    }

    /// Pick the item `step` below (above, if negative) the current one.
    pub fn move_by(&mut self, step: isize) {
        self.select(self.selected.saturating_add_signed(step));
    }

    /// Show the results of a search.
    pub fn set_rows(&mut self, rows: Vec<VaultRow>) {
        self.rows = rows;
        self.top = 0;
        self.selected = 0;
    }

    /// The vault was locked: forget the search and its results.
    pub fn lock(&mut self) {
        *self = Self::default();
    }
}

/// What a confirmation box is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmPurpose {
    /// Replace an existing file on the agent.
    Overwrite { local: PathBuf, remote: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub text: String,
    pub ok: String,
    pub purpose: ConfirmPurpose,
}

/// A menu or dialog over everything else. While one is open, clicks and
/// keys go to it, not to the remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Menu(Menu),
    Prompt(Prompt),
    Confirm(Confirm),
}

/// A message in the corner of the picture, until `until`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    pub until: Instant,
}

/// The toolbar's state.
#[derive(Debug, Clone, Copy)]
pub struct Toolbar<'a> {
    pub display: DisplayMode,
    /// The technician's frame-rate choice.
    pub frame_rate: FrameRate,
    /// What the agent streams at now, once it has said (for Auto, the rate
    /// it picked).
    pub streamed_fps: Option<u32>,
    pub monitors: &'a [MonitorInfo],
    pub active_monitor: Option<u32>,
    pub panel: bool,
    /// Text size, as chosen (before the scale factor).
    pub font_px: u32,
    /// Frame rate, path and the like, if there is room.
    pub info: &'a str,
}

/// What the panel knows about the agent.
#[derive(Debug, Clone, Copy)]
pub struct Panel<'a> {
    /// Whether the viewer can reach the API at all (it was given a token).
    pub api: bool,
    pub details: Option<&'a AgentDetails>,
    /// Why the last status refresh failed, if it did.
    pub error: Option<&'a str>,
    pub commands: &'a [QuickCommand],
    /// A file transfer in progress, described.
    pub busy: Option<&'a str>,
    pub fullscreen: bool,
}

/// Something drawn in the panel.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Text {
        x: u32,
        y: u32,
        color: u32,
        text: String,
    },
    /// A usage bar, `fraction` (0-1) full.
    Bar {
        rect: Rect,
        fraction: f64,
    },
    Button(Button),
    /// The vault's search field. A click gives it the keyboard.
    Field {
        rect: Rect,
        text: String,
        focused: bool,
    },
    /// An item the vault search found: its name, and its username or
    /// address beneath. A click picks it.
    Row {
        rect: Rect,
        /// Its place in the results.
        index: usize,
        name: String,
        detail: String,
        selected: bool,
    },
}

impl Item {
    /// Where it is, if it is a box (text has only a top-left corner).
    fn rect_mut(&mut self) -> Option<&mut Rect> {
        match self {
            Item::Text { .. } => None,
            Item::Bar { rect, .. } | Item::Field { rect, .. } | Item::Row { rect, .. } => {
                Some(rect)
            }
            Item::Button(b) => Some(&mut b.rect),
        }
    }

    fn bottom(&self, m: &Metrics) -> u32 {
        match self {
            Item::Text { y, .. } => y + m.glyph_h(),
            Item::Bar { rect, .. } | Item::Field { rect, .. } | Item::Row { rect, .. } => {
                rect.bottom()
            }
            Item::Button(b) => b.rect.bottom(),
        }
    }

    fn top(&self) -> u32 {
        match self {
            Item::Text { y, .. } => *y,
            Item::Bar { rect, .. } | Item::Field { rect, .. } | Item::Row { rect, .. } => rect.y,
            Item::Button(b) => b.rect.y,
        }
    }

    /// What a click on it does, if anything, and where.
    fn target(&self) -> Option<(Rect, Action)> {
        match self {
            Item::Button(b) if b.enabled => Some((b.rect, b.action.clone())),
            Item::Field { rect, .. } => Some((*rect, Action::VaultFocus)),
            Item::Row { rect, index, .. } => Some((*rect, Action::VaultSelect(*index))),
            _ => None,
        }
    }
}

/// What a click landed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Action(Action),
    /// The remote picture's area (with nothing open over it).
    Desktop,
    /// Outside the open menu: close it.
    Dismiss,
    /// The controls, but nothing that does anything.
    Nothing,
}

/// Everything the viewer draws besides the remote picture.
#[derive(Clone, Copy)]
pub struct Chrome<'a> {
    pub metrics: Metrics,
    pub layout: Layout,
    pub toolbar: Toolbar<'a>,
    pub panel: Panel<'a>,
    /// How far the panel is scrolled, in pixels.
    pub scroll: u32,
    pub overlay: Option<&'a Overlay>,
    /// The vault section of the panel.
    pub vault: &'a Vault,
    pub notices: &'a [Notice],
    /// Where the mouse is (for hover highlights).
    pub cursor: Option<(f64, f64)>,
}

/// `text`, cut to `max` characters with `..` when longer.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(2)).collect();
    format!("{kept}..")
}

/// `text` in lines of at most `width` characters, broken at spaces where
/// possible.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        loop {
            let used = line.chars().count();
            // A full line has no room for a space and another word.
            let room = if used == 0 {
                width
            } else {
                width.saturating_sub(used + 1)
            };
            if word.len() <= room {
                if used > 0 {
                    line.push(' ');
                }
                line.extend(&word);
                break;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            // A word longer than a line: split it.
            let rest = word.split_off(width);
            lines.push(word.into_iter().collect());
            word = rest;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `1.5 GiB`, `320.0 MiB`.
pub fn bytes(n: u64) -> String {
    let mut value = n as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 || unit == "TiB" {
            return if unit == "B" {
                format!("{n} B")
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    unreachable!()
}

/// A monitor as the menu lists it.
pub fn monitor_label(index: usize, m: &MonitorInfo) -> String {
    format!(
        "{}: {} {}x{}{}",
        index + 1,
        m.name,
        m.width,
        m.height,
        if m.primary { " (primary)" } else { "" }
    )
}

/// The Display menu.
pub fn display_menu(anchor: Rect, current: DisplayMode) -> Menu {
    Menu {
        anchor,
        items: DisplayMode::ALL
            .into_iter()
            .map(|mode| MenuItem {
                label: mode.description().into(),
                action: Action::SetDisplay(mode),
                checked: mode == current,
            })
            .collect(),
    }
}

/// The FPS button's label: the choice, and for Auto what it came to.
pub fn frame_rate_label(choice: FrameRate, streamed: Option<u32>) -> String {
    match (choice, streamed) {
        (FrameRate::Auto, Some(fps)) => format!("FPS: Auto ({fps})"),
        _ => format!("FPS: {choice}"),
    }
}

/// The FPS menu.
pub fn frame_rate_menu(anchor: Rect, current: FrameRate, streamed: Option<u32>) -> Menu {
    Menu {
        anchor,
        items: FrameRate::CHOICES
            .into_iter()
            .map(|choice| MenuItem {
                label: match choice {
                    FrameRate::Auto => match (current, streamed) {
                        (FrameRate::Auto, Some(fps)) => {
                            format!("Auto: by network speed (now {fps})")
                        }
                        _ => "Auto: by network speed".into(),
                    },
                    FrameRate::Max => "Max: every screen change".into(),
                    FrameRate::Fixed(fps) => format!("{fps} fps"),
                },
                action: Action::SetFrameRate(choice),
                checked: choice == current,
            })
            .collect(),
    }
}

/// Remember `value` under `key` in the JSON object in `path`, keeping its
/// other keys: `{"font_size": 9, "display": "fill"}`. Whoever starts the
/// next viewer (the TUI) reads the text size from it, and the viewer the
/// display mode. Written beside it and renamed, so a reader never sees
/// half a file.
pub fn save_setting(
    path: &std::path::Path,
    key: &str,
    value: serde_json::Value,
) -> std::io::Result<()> {
    let mut settings = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    };
    settings.insert(key.to_owned(), value);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::Value::Object(settings).to_string() + "\n")?;
    std::fs::rename(&tmp, path)
}

/// The display mode remembered in `path` (see [`save_setting`]), if any.
pub fn load_display(path: &std::path::Path) -> Option<DisplayMode> {
    let text = std::fs::read_to_string(path).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
    settings.get("display")?.as_str()?.parse().ok()
}

/// The Text menu: sizes to pick from, the current one marked.
pub fn text_menu(anchor: Rect, current: u32) -> Menu {
    let mut sizes = TEXT_SIZES.to_vec();
    if !sizes.contains(&current) {
        sizes.push(current);
        sizes.sort_unstable();
    }
    Menu {
        anchor,
        items: sizes
            .into_iter()
            .map(|px| MenuItem {
                label: if px == DEFAULT_FONT_PX {
                    format!("{px} px (default)")
                } else {
                    format!("{px} px")
                },
                action: Action::SetTextSize(px),
                checked: px == current,
            })
            .collect(),
    }
}

/// The Monitor menu.
pub fn monitor_menu(anchor: Rect, monitors: &[MonitorInfo], active: Option<u32>) -> Menu {
    let mut items: Vec<MenuItem> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| MenuItem {
            label: monitor_label(i, m),
            action: Action::SelectMonitor(m.id),
            checked: Some(m.id) == active,
        })
        .collect();
    if items.is_empty() {
        items.push(MenuItem {
            label: "(waiting for the monitor list)".into(),
            action: Action::MonitorMenu,
            checked: false,
        });
    }
    Menu { anchor, items }
}

impl Chrome<'_> {
    fn m(&self) -> &Metrics {
        &self.metrics
    }

    // --- toolbar ------------------------------------------------------------

    fn toolbar_button(&self, x: u32, label: &str, action: Action, dropdown: bool) -> Button {
        let m = self.m();
        let extra = if dropdown { m.char_w() + m.pad } else { 0 };
        let w = render::text_width(label, m.font) + 2 * m.pad + extra;
        let rect = Rect::new(x, self.layout.toolbar.y + m.gap, w, m.button_h());
        Button {
            dropdown,
            ..Button::new(rect, label, action)
        }
    }

    pub fn toolbar_buttons(&self) -> Vec<Button> {
        let m = *self.m();
        let t = &self.toolbar;
        let mut buttons = Vec::new();
        let mut x = m.gap;
        let mut add_left =
            |label: &str, action: Action, dropdown: bool, buttons: &mut Vec<Button>| {
                let b = self.toolbar_button(x, label, action, dropdown);
                x = b.rect.right() + m.gap;
                buttons.push(b);
            };
        add_left(
            &format!("Display: {}", t.display.label()),
            Action::DisplayMenu,
            true,
            &mut buttons,
        );
        add_left(
            &frame_rate_label(t.frame_rate, t.streamed_fps),
            Action::FrameRateMenu,
            true,
            &mut buttons,
        );
        let monitor = match t
            .active_monitor
            .and_then(|a| t.monitors.iter().position(|mon| mon.id == a))
        {
            Some(i) if t.monitors.len() > 1 => format!("Monitor {} of {}", i + 1, t.monitors.len()),
            _ => "Monitor".to_owned(),
        };
        add_left(&monitor, Action::MonitorMenu, true, &mut buttons);
        add_left(
            &format!("Text: {}", t.font_px),
            Action::TextMenu,
            true,
            &mut buttons,
        );
        // The panel's toggle, at the right edge.
        let mut panel = self.toolbar_button(0, "Panel", Action::TogglePanel, false);
        let edge = self.layout.toolbar.right().saturating_sub(m.gap);
        panel.rect.x = edge.saturating_sub(panel.rect.w);
        panel.active = t.panel;
        let right = panel.rect.x.saturating_sub(m.gap);
        // A narrow window: leave out buttons from the end of the left group
        // (text size has shortcuts, monitors too) rather than overlap the
        // right group. The first two always stay.
        while buttons.len() > 2 && buttons.last().is_some_and(|b| b.rect.right() > right) {
            buttons.pop();
        }
        buttons.push(panel);
        buttons
    }

    // --- panel --------------------------------------------------------------

    /// The panel's contents in window coordinates (scrolled), and the height
    /// of all of it.
    pub fn panel_items(&self) -> (Vec<Item>, u32) {
        let Some(panel) = self.layout.panel else {
            return (Vec::new(), 0);
        };
        let m = *self.m();
        let p = &self.panel;
        let x = panel.x + m.gap + m.pad;
        let width = panel.w - 2 * (m.gap + m.pad);
        let chars = (width / m.char_w()) as usize;
        let top = panel.y + m.gap;
        let mut y = top;
        let mut items = Vec::new();
        let text = |items: &mut Vec<Item>, y: &mut u32, indent: u32, color: u32, s: &str| {
            let cols = chars.saturating_sub(indent as usize);
            for line in wrap(s, cols) {
                items.push(Item::Text {
                    x: x + indent * m.char_w(),
                    y: *y,
                    color,
                    text: line,
                });
                *y += m.line_h();
            }
        };
        let heading = |items: &mut Vec<Item>, y: &mut u32, s: &str| {
            *y += m.gap;
            items.push(Item::Text {
                x,
                y: *y,
                color: colors::HEADING,
                text: s.into(),
            });
            *y += m.line_h() + m.gap;
        };
        let button = |items: &mut Vec<Item>, y: &mut u32, label: &str, action, enabled| {
            let rect = Rect::new(panel.x + m.gap, *y, panel.w - 2 * m.gap, m.button_h());
            items.push(Item::Button(Button {
                enabled,
                ..Button::new(rect, truncate(label, chars), action)
            }));
            *y += m.button_h() + m.gap;
        };

        // Buttons side by side, sharing the panel's width equally.
        let row = |items: &mut Vec<Item>, y: &mut u32, buttons: &[(&str, Action, bool, bool)]| {
            let n = buttons.len() as u32;
            let full = panel.w - 2 * m.gap;
            let w = full.saturating_sub((n - 1) * m.gap) / n;
            let room = (w.saturating_sub(2 * m.pad) / m.char_w()) as usize;
            for (i, (label, action, enabled, active)) in buttons.iter().enumerate() {
                let rect = Rect::new(
                    panel.x + m.gap + i as u32 * (w + m.gap),
                    *y,
                    w,
                    m.button_h(),
                );
                items.push(Item::Button(Button {
                    enabled: *enabled,
                    active: *active,
                    ..Button::new(rect, truncate(label, room), action.clone())
                }));
            }
            *y += m.button_h() + m.gap;
        };

        heading(&mut items, &mut y, "SESSION");
        row(
            &mut items,
            &mut y,
            &[
                ("Disconnect", Action::Disconnect, true, false),
                ("Full screen", Action::ToggleFullscreen, true, p.fullscreen),
            ],
        );
        row(
            &mut items,
            &mut y,
            &[
                ("Refresh", Action::Keyframe, true, false),
                ("Ctrl+Alt+Del", Action::SecureAttention, true, false),
            ],
        );

        heading(&mut items, &mut y, "VAULT");
        let v = self.vault;
        let idle = v.busy.is_none();
        if v.unlocked {
            items.push(Item::Field {
                rect: Rect::new(panel.x + m.gap, y, panel.w - 2 * m.gap, m.button_h()),
                text: v.query.clone(),
                focused: v.focused,
            });
            y += m.button_h() + m.gap;
            let row_h = 2 * m.line_h() + m.gap;
            for index in (v.top..v.rows.len()).take(VAULT_ROWS) {
                let found = &v.rows[index];
                items.push(Item::Row {
                    rect: Rect::new(panel.x + m.gap, y, panel.w - 2 * m.gap, row_h),
                    index,
                    name: truncate(&found.name, chars),
                    detail: truncate(&found.detail, chars),
                    selected: index == v.selected,
                });
                y += row_h;
            }
            if !v.rows.is_empty() {
                y += m.gap;
            }
            let has = |kind| idle && v.current().is_some_and(|found| found.has(kind));
            row(
                &mut items,
                &mut y,
                &[
                    (
                        "Username",
                        Action::VaultType(TextKind::Username),
                        has(TextKind::Username),
                        false,
                    ),
                    (
                        "Password",
                        Action::VaultType(TextKind::Password),
                        has(TextKind::Password),
                        false,
                    ),
                    (
                        "TOTP",
                        Action::VaultType(TextKind::Totp),
                        has(TextKind::Totp),
                        false,
                    ),
                ],
            );
            row(
                &mut items,
                &mut y,
                &[
                    ("Sync", Action::VaultSync, idle, false),
                    ("Lock", Action::VaultLock, idle, false),
                ],
            );
        } else {
            button(
                &mut items,
                &mut y,
                "Unlock vault",
                Action::VaultUnlock,
                idle,
            );
        }
        let more = v.rows.len().saturating_sub(VAULT_ROWS);
        match (&v.busy, &v.note) {
            (Some(busy), _) => text(&mut items, &mut y, 0, colors::MUTED, busy),
            (None, Some((note, true))) => text(&mut items, &mut y, 0, colors::WARN, note),
            (None, Some((note, false))) => text(&mut items, &mut y, 0, colors::MUTED, note),
            (None, None) if more > 0 => text(
                &mut items,
                &mut y,
                0,
                colors::MUTED,
                &format!("{} found: scroll for more.", v.rows.len()),
            ),
            (None, None) if v.unlocked && v.rows.is_empty() => text(
                &mut items,
                &mut y,
                0,
                colors::MUTED,
                "Click the field, type a name and press Enter.",
            ),
            (None, None) => {}
        }

        // The password the remote user lends (see `protocol::credential`).
        // It is kept on their machine, so the viewer cannot tell whether
        // there is one: every choice is always there, and the agent says
        // what came of it.
        heading(&mut items, &mut y, "LENT PASSWORD");
        row(
            &mut items,
            &mut y,
            &[
                ("Ask user", Action::PasswordRequest, true, false),
                ("Type", Action::PasswordType, true, false),
                ("Forget", Action::PasswordForget, true, false),
            ],
        );

        heading(&mut items, &mut y, "STATUS");
        match (p.api, p.details) {
            (false, _) => text(
                &mut items,
                &mut y,
                0,
                colors::MUTED,
                "Start the viewer from the TUI to see the agent's status here and use the \
                 commands and file transfer.",
            ),
            (true, None) => {
                let note = p.error.unwrap_or("Loading...");
                text(&mut items, &mut y, 0, colors::MUTED, note);
            }
            (true, Some(d)) => {
                // Label muted, values bright below it at full width (a
                // DOMAIN\user or IPv6 address needs every column), then a gap.
                let mut field = |label: &str, values: Vec<String>| {
                    text(&mut items, &mut y, 0, colors::MUTED, label);
                    for value in values {
                        text(&mut items, &mut y, 0, colors::TEXT, &value);
                    }
                    y += m.gap;
                };
                let or_unknown = |v: &Option<String>| vec![v.clone().unwrap_or_else(|| "-".into())];
                field("Hostname", or_unknown(&d.hostname));
                field(
                    "Signed in",
                    match &d.logged_in_users {
                        None => vec!["-".into()],
                        Some(users) if users.is_empty() => vec!["nobody".into()],
                        Some(users) => users.clone(),
                    },
                );
                field("Internal IP", or_unknown(&d.local_ip));
                field("External IP", or_unknown(&d.remote_ip));
                field(
                    "DNS servers",
                    match &d.dns_servers {
                        Some(list) if !list.is_empty() => list.clone(),
                        Some(_) => vec!["none".into()],
                        None => vec!["-".into()],
                    },
                );
                field("OS", or_unknown(&d.os));
                text(&mut items, &mut y, 0, colors::MUTED, "Disks");
                match &d.disks {
                    Some(disks) if !disks.is_empty() => {
                        for disk in disks {
                            let fraction = if disk.total_bytes == 0 {
                                0.0
                            } else {
                                disk.used_bytes as f64 / disk.total_bytes as f64
                            };
                            let percent = format!("{:.0}%", fraction * 100.0);
                            let name_room = chars.saturating_sub(percent.len() + 2);
                            let name = truncate(&disk.name, name_room);
                            let padding =
                                chars.saturating_sub(name.chars().count() + percent.len());
                            // One line, the percentage right-aligned: not wrapped.
                            items.push(Item::Text {
                                x,
                                y,
                                color: colors::TEXT,
                                text: format!("{name}{}{percent}", " ".repeat(padding.max(1))),
                            });
                            y += m.line_h();
                            items.push(Item::Bar {
                                rect: Rect::new(x, y, width, m.bar_h()),
                                fraction,
                            });
                            y += m.bar_h() + m.gap;
                            text(
                                &mut items,
                                &mut y,
                                0,
                                colors::MUTED,
                                &format!(
                                    "{} / {}",
                                    bytes(disk.used_bytes),
                                    bytes(disk.total_bytes)
                                ),
                            );
                        }
                    }
                    _ => text(&mut items, &mut y, 0, colors::TEXT, "-"),
                }
                if !d.online {
                    text(&mut items, &mut y, 0, colors::WARN, "The agent is offline.");
                }
                if let Some(error) = p.error {
                    text(
                        &mut items,
                        &mut y,
                        0,
                        colors::WARN,
                        &format!("Not updating: {error}"),
                    );
                }
            }
        }

        heading(&mut items, &mut y, "COMMANDS");
        let can_launch = p.details.is_some_and(|d| d.online && d.can("desktop"));
        if p.commands.is_empty() {
            text(
                &mut items,
                &mut y,
                0,
                colors::MUTED,
                "None: add some in the TUI.",
            );
        }
        for (i, command) in p.commands.iter().enumerate() {
            button(
                &mut items,
                &mut y,
                &command.label,
                Action::Launch(i),
                can_launch,
            );
        }

        heading(&mut items, &mut y, "FILES");
        let can_transfer = p.busy.is_none()
            && p.details
                .is_some_and(|d| d.online && d.can("file_transfer"));
        button(
            &mut items,
            &mut y,
            "Upload file...",
            Action::Upload,
            can_transfer,
        );
        button(
            &mut items,
            &mut y,
            "Download file...",
            Action::Download,
            can_transfer,
        );
        if let Some(busy) = p.busy {
            text(&mut items, &mut y, 0, colors::MUTED, busy);
        } else if p.details.is_some_and(|d| !d.can("file_transfer")) {
            text(&mut items, &mut y, 0, colors::MUTED, "No file access here.");
        }

        let height = y - top + m.gap;
        for item in &mut items {
            if let Item::Text { y, .. } = item {
                *y = y.wrapping_sub(self.scroll);
            } else if let Some(rect) = item.rect_mut() {
                rect.y = rect.y.wrapping_sub(self.scroll);
            }
        }
        (items, height)
    }

    /// Furthest the panel scrolls.
    pub fn max_scroll(&self) -> u32 {
        let Some(panel) = self.layout.panel else {
            return 0;
        };
        let (_, height) = Chrome { scroll: 0, ..*self }.panel_items();
        height.saturating_sub(panel.h)
    }

    /// Whether `item` lies entirely in the visible part of the panel.
    fn visible(&self, item: &Item) -> bool {
        let Some(panel) = self.layout.panel else {
            return false;
        };
        let top = item.top();
        // Scrolled off the top wraps around to a huge value.
        top >= panel.y && top < panel.bottom() && item.bottom(self.m()) <= panel.bottom()
    }

    // --- overlays -----------------------------------------------------------

    /// A menu's box, and each item's row.
    pub fn menu_rects(&self, menu: &Menu) -> (Rect, Vec<Rect>) {
        let m = self.m();
        let chars = menu
            .items
            .iter()
            .map(|i| i.label.chars().count() as u32 + 2)
            .max()
            .unwrap_or(0);
        let w = (chars * m.char_w() + 2 * m.pad).min(self.layout.window.w);
        let x = menu.anchor.x.min(self.layout.window.w.saturating_sub(w));
        let y = menu.anchor.bottom() + m.border();
        let rows: Vec<Rect> = (0..menu.items.len() as u32)
            .map(|i| Rect::new(x, y + i * m.button_h(), w, m.button_h()))
            .collect();
        let h = rows.len() as u32 * m.button_h();
        (Rect::new(x, y, w, h), rows)
    }

    /// A dialog's box, its lines of text, and its buttons (the prompt also
    /// has a text field).
    fn dialog(&self, text: &str, buttons: [(&str, Action); 2], field: bool) -> DialogGeometry {
        let m = *self.m();
        let area = self.layout.desktop;
        let chars = PROMPT_CHARS
            .min(area.w.saturating_sub(2 * (m.gap + m.pad)) / m.char_w())
            .max(8);
        let w = chars * m.char_w() + 2 * m.pad;
        let lines = wrap(text, chars as usize);
        let field_h = if field { m.button_h() + m.gap } else { 0 };
        let h = m.pad + lines.len() as u32 * m.line_h() + m.gap + field_h + m.button_h() + m.pad;
        let x = area.x + area.w.saturating_sub(w) / 2;
        let y = area.y + area.h.saturating_sub(h) / 3;
        let bx = Rect::new(x, y, w, h);
        let field_y = y + m.pad + lines.len() as u32 * m.line_h() + m.gap;
        let field = field.then(|| Rect::new(x + m.pad, field_y, w - 2 * m.pad, m.button_h()));
        let buttons_y = y + h - m.pad - m.button_h();
        let mut right = bx.right() - m.pad;
        let mut rects = Vec::new();
        for (label, action) in buttons {
            let bw = render::text_width(label, m.font) + 2 * m.pad;
            let rect = Rect::new(right - bw, buttons_y, bw, m.button_h());
            right = rect.x - m.gap;
            rects.push(Button::new(rect, label, action));
        }
        DialogGeometry {
            rect: bx,
            lines,
            field,
            buttons: rects,
        }
    }

    fn overlay_geometry(&self, overlay: &Overlay) -> Option<DialogGeometry> {
        match overlay {
            Overlay::Menu(_) => None,
            Overlay::Prompt(p) => Some(self.dialog(
                &p.title,
                [("OK", Action::PromptOk), ("Cancel", Action::PromptCancel)],
                true,
            )),
            Overlay::Confirm(c) => Some(self.dialog(
                &c.text,
                [
                    (c.ok.as_str(), Action::ConfirmOk),
                    ("Cancel", Action::ConfirmCancel),
                ],
                false,
            )),
        }
    }

    // --- hit testing --------------------------------------------------------

    /// What a click at `pos` does.
    pub fn hit(&self, pos: (f64, f64)) -> Hit {
        let enabled = |b: &Button| (b.enabled && b.rect.contains(pos)).then(|| b.action.clone());
        if let Some(overlay) = self.overlay {
            return match overlay {
                Overlay::Menu(menu) => {
                    let (_, rows) = self.menu_rects(menu);
                    match rows.iter().position(|r| r.contains(pos)) {
                        Some(i) => Hit::Action(menu.items[i].action.clone()),
                        None => Hit::Dismiss,
                    }
                }
                _ => {
                    let geometry = self.overlay_geometry(overlay).expect("a dialog");
                    geometry
                        .buttons
                        .iter()
                        .find_map(enabled)
                        .map_or(Hit::Nothing, Hit::Action)
                }
            };
        }
        if self.layout.toolbar.contains(pos) {
            return self
                .toolbar_buttons()
                .iter()
                .find_map(enabled)
                .map_or(Hit::Nothing, Hit::Action);
        }
        if self.layout.panel.is_some_and(|p| p.contains(pos)) {
            let (items, _) = self.panel_items();
            return items
                .iter()
                .filter(|i| self.visible(i))
                .filter_map(Item::target)
                .find(|(rect, _)| rect.contains(pos))
                .map_or(Hit::Nothing, |(_, action)| Hit::Action(action));
        }
        if self.layout.desktop.contains(pos) {
            return Hit::Desktop;
        }
        Hit::Nothing
    }

    /// Whether anything under the cursor would change its highlight.
    pub fn hover_target(&self) -> Option<Rect> {
        let pos = self.cursor?;
        if let Some(Overlay::Menu(menu)) = self.overlay {
            let (_, rows) = self.menu_rects(menu);
            return rows.into_iter().find(|r| r.contains(pos));
        }
        if let Some(overlay) = self.overlay {
            let geometry = self.overlay_geometry(overlay)?;
            return geometry
                .buttons
                .iter()
                .map(|b| b.rect)
                .find(|r| r.contains(pos));
        }
        let (items, _) = self.panel_items();
        self.toolbar_buttons()
            .into_iter()
            .filter(|b| b.enabled)
            .map(|b| b.rect)
            .chain(
                items
                    .iter()
                    .filter(|i| self.visible(i))
                    .filter_map(|i| i.target().map(|(rect, _)| rect)),
            )
            .find(|r| r.contains(pos))
    }

    /// Whether `pos` is over the vault's list of items (the wheel scrolls
    /// it rather than the panel).
    pub fn over_vault_rows(&self, pos: (f64, f64)) -> bool {
        let (items, _) = self.panel_items();
        items
            .iter()
            .filter(|i| self.visible(i))
            .any(|i| matches!(i, Item::Row { rect, .. } if rect.contains(pos)))
    }

    // --- drawing ------------------------------------------------------------

    fn draw_button(&self, canvas: &mut Canvas, b: &Button) {
        let m = self.m();
        let hovered = b.enabled && self.cursor.is_some_and(|c| b.rect.contains(c));
        let background = if b.active {
            colors::ACTIVE
        } else if hovered {
            colors::HOVER
        } else {
            colors::BUTTON
        };
        render::fill_rect(canvas, b.rect, background);
        render::outline(canvas, b.rect, m.border(), colors::BORDER);
        let color = if b.enabled {
            colors::TEXT
        } else {
            colors::DISABLED
        };
        render::draw_text(
            canvas,
            (b.rect.x + m.pad, m.text_y(b.rect)),
            m.font,
            color,
            &b.label,
        );
        if b.dropdown {
            let size = m.char_w() * 3 / 4;
            let x = b.rect.right() - m.pad - size;
            let y = b.rect.y + (b.rect.h - size / 2) / 2;
            render::draw_caret(canvas, (x, y), size, color);
        }
    }

    /// A text field holding `text`; with the keyboard (`focused`), outlined
    /// and with the caret after the text.
    fn draw_field(&self, canvas: &mut Canvas, field: Rect, text: &str, focused: bool) {
        let m = self.m();
        render::fill_rect(canvas, field, colors::FIELD);
        let edge = if focused {
            colors::ACTIVE
        } else {
            colors::BORDER
        };
        render::outline(canvas, field, m.border(), edge);
        // Show the end of a long entry: that is where typing goes.
        let room = (field.w.saturating_sub(2 * m.pad) / m.char_w()).saturating_sub(1) as usize;
        let count = text.chars().count();
        let shown: String = text.chars().skip(count.saturating_sub(room)).collect();
        let tx = field.x + m.pad;
        let ty = m.text_y(field);
        render::draw_text(canvas, (tx, ty), m.font, colors::TEXT, &shown);
        if focused {
            let caret_x = tx + render::text_width(&shown, m.font);
            render::fill_rect(
                canvas,
                Rect::new(caret_x, ty, m.border(), m.glyph_h()),
                colors::TEXT,
            );
        }
    }

    fn draw_notices(&self, canvas: &mut Canvas) {
        let m = *self.m();
        let area = self.layout.desktop;
        let chars = NOTICE_CHARS.min((area.w.saturating_sub(4 * m.gap) / m.char_w()) as usize);
        let mut bottom = area.bottom().saturating_sub(m.gap);
        for notice in self.notices.iter().rev() {
            let lines = wrap(&notice.text, chars.max(8));
            let w = lines
                .iter()
                .map(|l| l.chars().count() as u32)
                .max()
                .unwrap_or(0)
                * m.char_w()
                + 2 * m.pad;
            let h = lines.len() as u32 * m.line_h() - m.gap + 2 * m.pad;
            if bottom < area.y + h {
                break;
            }
            let rect = Rect::new(area.x + m.gap, bottom - h, w, h);
            let background = if notice.error {
                colors::ERROR
            } else {
                colors::NOTICE
            };
            render::fill_rect(canvas, rect, background);
            render::outline(canvas, rect, m.border(), colors::BORDER);
            for (i, line) in lines.iter().enumerate() {
                let y = rect.y + m.pad + i as u32 * m.line_h();
                render::draw_text(canvas, (rect.x + m.pad, y), m.font, colors::TEXT, line);
            }
            bottom = rect.y.saturating_sub(m.gap);
        }
    }

    /// Draw the controls over a canvas that already holds the picture.
    pub fn draw(&self, canvas: &mut Canvas) {
        let m = *self.m();
        let l = self.layout;

        // Toolbar.
        render::fill_rect(canvas, l.toolbar, colors::TOOLBAR);
        render::fill_rect(
            canvas,
            Rect::new(
                0,
                l.toolbar.bottom().saturating_sub(m.border()),
                l.toolbar.w,
                m.border(),
            ),
            colors::BORDER,
        );
        let buttons = self.toolbar_buttons();
        for b in &buttons {
            self.draw_button(canvas, b);
        }
        // The info text goes between the two groups, if it fits.
        let left_end = buttons
            .iter()
            .filter(|b| b.rect.x < l.toolbar.w / 2)
            .map(|b| b.rect.right())
            .max()
            .unwrap_or(0);
        let right_start = buttons
            .iter()
            .filter(|b| b.rect.x >= l.toolbar.w / 2)
            .map(|b| b.rect.x)
            .min()
            .unwrap_or(l.toolbar.w);
        let room = right_start.saturating_sub(left_end + 4 * m.gap);
        if !self.toolbar.info.is_empty() && render::text_width(self.toolbar.info, m.font) <= room {
            let w = render::text_width(self.toolbar.info, m.font);
            let x = left_end + 2 * m.gap + (room - w) / 2;
            render::draw_text(
                canvas,
                (x, m.text_y(l.toolbar)),
                m.font,
                colors::MUTED,
                self.toolbar.info,
            );
        }

        // Panel.
        if let Some(panel) = l.panel {
            render::fill_rect(canvas, panel, colors::PANEL);
            render::fill_rect(
                canvas,
                Rect::new(panel.x, panel.y, m.border(), panel.h),
                colors::BORDER,
            );
            let (items, height) = self.panel_items();
            for item in items.iter().filter(|i| self.visible(i)) {
                match item {
                    Item::Text { x, y, color, text } => {
                        render::draw_text(canvas, (*x, *y), m.font, *color, text)
                    }
                    Item::Bar { rect, fraction } => {
                        render::fill_rect(canvas, *rect, colors::BAR);
                        let fill = (f64::from(rect.w) * fraction.clamp(0.0, 1.0)).round() as u32;
                        let color = match fraction {
                            f if *f >= 0.9 => colors::FULL,
                            f if *f >= 0.8 => colors::WARN,
                            _ => colors::OK,
                        };
                        render::fill_rect(canvas, Rect { w: fill, ..*rect }, color);
                    }
                    Item::Button(b) => self.draw_button(canvas, b),
                    Item::Field {
                        rect,
                        text,
                        focused,
                    } => self.draw_field(canvas, *rect, text, *focused),
                    Item::Row {
                        rect,
                        name,
                        detail,
                        selected,
                        ..
                    } => {
                        if *selected {
                            render::fill_rect(canvas, *rect, colors::ACTIVE);
                        } else if self.cursor.is_some_and(|c| rect.contains(c)) {
                            render::fill_rect(canvas, *rect, colors::HOVER);
                        }
                        let tx = rect.x + m.pad;
                        let ty = rect.y + m.gap;
                        render::draw_text(canvas, (tx, ty), m.font, colors::TEXT, name);
                        let ty = ty + m.line_h();
                        render::draw_text(canvas, (tx, ty), m.font, colors::MUTED, detail);
                    }
                }
            }
            // A scroll bar when it does not all fit.
            if height > panel.h {
                let track = panel.h;
                let thumb = (u64::from(track) * u64::from(panel.h) / u64::from(height)) as u32;
                let top = (u64::from(track - thumb) * u64::from(self.scroll)
                    / u64::from((height - panel.h).max(1))) as u32;
                let w = m.border() * 2;
                render::fill_rect(
                    canvas,
                    Rect::new(panel.right() - w, panel.y + top, w, thumb.max(m.gap)),
                    colors::HOVER,
                );
            }
        }

        self.draw_notices(canvas);

        // Menus and dialogs, on top.
        match self.overlay {
            Some(Overlay::Menu(menu)) => {
                let (bx, rows) = self.menu_rects(menu);
                render::fill_rect(canvas, bx, colors::TOOLBAR);
                for (item, row) in menu.items.iter().zip(&rows) {
                    let hovered = self.cursor.is_some_and(|c| row.contains(c));
                    if hovered {
                        render::fill_rect(canvas, *row, colors::HOVER);
                    }
                    let mark = if item.checked { "* " } else { "  " };
                    render::draw_text(
                        canvas,
                        (row.x + m.pad, m.text_y(*row)),
                        m.font,
                        if item.checked {
                            colors::HEADING
                        } else {
                            colors::TEXT
                        },
                        &format!("{mark}{}", item.label),
                    );
                }
                render::outline(canvas, bx, m.border(), colors::BORDER);
            }
            Some(overlay @ (Overlay::Prompt(_) | Overlay::Confirm(_))) => {
                let g = self.overlay_geometry(overlay).expect("a dialog");
                render::fill_rect(canvas, g.rect, colors::TOOLBAR);
                render::outline(canvas, g.rect, m.border(), colors::ACTIVE);
                for (i, line) in g.lines.iter().enumerate() {
                    let y = g.rect.y + m.pad + i as u32 * m.line_h();
                    render::draw_text(canvas, (g.rect.x + m.pad, y), m.font, colors::TEXT, line);
                }
                if let (Some(field), Overlay::Prompt(prompt)) = (g.field, overlay) {
                    if prompt.masked {
                        let stars = "*".repeat(prompt.text.chars().count());
                        self.draw_field(canvas, field, &stars, true);
                    } else {
                        self.draw_field(canvas, field, &prompt.text, true);
                    }
                }
                for b in &g.buttons {
                    self.draw_button(canvas, b);
                }
            }
            None => {}
        }
    }
}

struct DialogGeometry {
    rect: Rect,
    lines: Vec<String>,
    field: Option<Rect>,
    buttons: Vec<Button>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Disk;

    const M: Metrics = Metrics {
        font: 16,
        pad: 5,
        gap: 4,
    };

    fn monitor(id: u32, primary: bool) -> MonitorInfo {
        MonitorInfo {
            id,
            name: format!(r"\\.\DISPLAY{}", id + 1),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
        }
    }

    fn details() -> AgentDetails {
        AgentDetails {
            id: "agt-1".into(),
            hostname: Some("WS-01".into()),
            online: true,
            os: Some("Windows 11 Pro (build 26100)".into()),
            local_ip: Some("192.168.1.20".into()),
            remote_ip: Some("203.0.113.9".into()),
            dns_servers: Some(vec!["192.168.1.1".into(), "8.8.8.8".into()]),
            logged_in_users: Some(vec!["CORP\\alice".into()]),
            disks: Some(vec![Disk {
                name: "C:\\".into(),
                total_bytes: 250 << 30,
                used_bytes: 112 << 30,
            }]),
            capabilities: vec!["desktop".into(), "file_transfer".into()],
        }
    }

    struct Fixture {
        monitors: Vec<MonitorInfo>,
        details: AgentDetails,
        commands: Vec<QuickCommand>,
        overlay: Option<Overlay>,
        vault: Vault,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                monitors: vec![monitor(0, true), monitor(1, false)],
                details: details(),
                commands: default_commands(),
                overlay: None,
                vault: Vault::default(),
            }
        }

        fn chrome(&self, width: u32, height: u32) -> Chrome<'_> {
            Chrome {
                metrics: M,
                layout: layout(width, height, &M, true),
                toolbar: Toolbar {
                    display: DisplayMode::Fill,
                    frame_rate: FrameRate::Auto,
                    streamed_fps: Some(60),
                    monitors: &self.monitors,
                    active_monitor: Some(1),
                    panel: true,
                    font_px: 16,
                    info: "30 fps",
                },
                panel: Panel {
                    api: true,
                    details: Some(&self.details),
                    error: None,
                    commands: &self.commands,
                    busy: None,
                    fullscreen: false,
                },
                scroll: 0,
                overlay: self.overlay.as_ref(),
                vault: &self.vault,
                notices: &[],
                cursor: None,
            }
        }
    }

    fn centre(r: Rect) -> (f64, f64) {
        (f64::from(r.x + r.w / 2), f64::from(r.y + r.h / 2))
    }

    fn texts(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::Text { text, .. } => Some(text.trim().to_owned()),
                Item::Button(b) => Some(format!("[{}]", b.label)),
                Item::Field { text, .. } => Some(format!("<{text}>")),
                Item::Row { name, detail, .. } => Some(format!("{name} | {detail}")),
                Item::Bar { .. } => None,
            })
            .collect()
    }

    #[test]
    fn layout_puts_the_panel_right_and_the_picture_in_between() {
        let l = layout(1600, 900, &M, true);
        assert_eq!(l.toolbar, Rect::new(0, 0, 1600, M.toolbar_h()));
        let panel = l.panel.unwrap();
        assert_eq!(
            (panel.right(), panel.y, panel.bottom()),
            (1600, M.toolbar_h(), 900)
        );
        assert_eq!(
            l.desktop,
            Rect::new(0, M.toolbar_h(), 1600 - panel.w, 900 - M.toolbar_h())
        );
        // Hidden, or no room for it: the picture takes the width.
        assert_eq!(layout(1600, 900, &M, false).desktop.w, 1600);
        assert_eq!(layout(500, 900, &M, true).panel, None);
    }

    #[test]
    fn metrics_follow_the_scale_factor() {
        assert_eq!(Metrics::for_scale_factor(1.0).font, DEFAULT_FONT_PX);
        assert_eq!(
            Metrics::new(12, 1.0),
            Metrics {
                font: 12,
                pad: 4,
                gap: 3
            }
        );
        assert_eq!(Metrics::new(12, 1.5).font, 18);
        assert_eq!(Metrics::new(12, 2.0).font, 24);
        assert_eq!(Metrics::new(12, 0.5).font, 12);
        // Buttons have room above and below the text, which sits centred.
        let m = Metrics::new(12, 1.0);
        assert_eq!(m.button_h(), 12 + 2 * (4 + 3));
        assert_eq!(m.text_y(Rect::new(0, 100, 50, m.button_h())), 107);
        // Out-of-range sizes are clamped.
        assert_eq!(Metrics::new(1, 1.0).font, MIN_FONT_PX);
        assert_eq!(Metrics::new(500, 1.0).font, MAX_FONT_PX);
    }

    #[test]
    fn the_text_size_and_display_mode_are_remembered_together() {
        let dir = std::env::temp_dir().join(format!("rmm-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("viewer.json");
        assert_eq!(load_display(&path), None);
        save_setting(&path, "font_size", 9.into()).unwrap();
        assert_eq!(load_display(&path), None);
        save_setting(&path, "display", "fill".into()).unwrap();
        save_setting(&path, "font_size", 11.into()).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({ "font_size": 11, "display": "fill" })
        );
        assert_eq!(load_display(&path), Some(DisplayMode::Fill));
        assert!(!path.with_extension("tmp").exists());
        // Every mode survives the round trip, as the viewer saves it.
        for mode in DisplayMode::ALL {
            save_setting(&path, "display", mode.label().to_lowercase().into()).unwrap();
            assert_eq!(load_display(&path), Some(mode));
        }
        // Nonsense in the file is no mode, and is replaced by the next save.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_display(&path), None);
        save_setting(&path, "display", "scale".into()).unwrap();
        assert_eq!(load_display(&path), Some(DisplayMode::Scale));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_menu_offers_sizes_and_marks_the_current_one() {
        let menu = text_menu(Rect::default(), 10);
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "8 px",
                "9 px",
                "10 px (default)",
                "11 px",
                "12 px",
                "14 px",
                "16 px",
                "20 px"
            ]
        );
        assert!(menu.items[2].checked && menu.items[2].action == Action::SetTextSize(10));
        // A size set on the command line shows up too.
        let menu = text_menu(Rect::default(), 13);
        assert!(menu.items.iter().any(|i| i.checked && i.label == "13 px"));
    }

    #[test]
    fn the_toolbar_holds_the_four_menus_and_the_panel_toggle() {
        let f = Fixture::new();
        let chrome = f.chrome(2000, 900);
        let buttons = chrome.toolbar_buttons();
        let labels: Vec<&str> = buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Display: Fill",
                "FPS: Auto (60)",
                "Monitor 2 of 2",
                "Text: 16",
                "Panel"
            ]
        );
        assert!(buttons[..4].iter().all(|b| b.dropdown) && buttons[4].active);
        // The toggle sits at the right edge.
        assert_eq!(buttons[4].rect.right(), 2000 - M.gap);
        let actions = [
            Action::DisplayMenu,
            Action::FrameRateMenu,
            Action::MonitorMenu,
            Action::TextMenu,
            Action::TogglePanel,
        ];
        for (button, action) in buttons.iter().zip(actions) {
            assert_eq!(chrome.hit(centre(button.rect)), Hit::Action(action));
        }
        assert_eq!(chrome.hit((800.0, 500.0)), Hit::Desktop);
    }

    #[test]
    fn a_narrow_toolbar_leaves_out_buttons_rather_than_overlapping() {
        let f = Fixture::new();
        let labels = |width| -> Vec<String> {
            f.chrome(width, 600)
                .toolbar_buttons()
                .into_iter()
                .map(|b| b.label)
                .collect()
        };
        let wide = labels(1600);
        let narrow = labels(700);
        assert!(narrow.len() < wide.len(), "{narrow:?}");
        assert_eq!(narrow[..2], wide[..2], "display and FPS stay");
        assert!(narrow.ends_with(&["Panel".to_owned()]));
        for width in [300, 500, 700, 900, 1200] {
            let buttons = f.chrome(width, 600).toolbar_buttons();
            let (left, right): (Vec<_>, Vec<_>) = buttons
                .iter()
                .partition(|b| b.action != Action::TogglePanel);
            let left_end = left.iter().map(|b| b.rect.right()).max().unwrap();
            let right_start = right.iter().map(|b| b.rect.x).min().unwrap();
            assert!(
                left.len() == 2 || left_end <= right_start,
                "{width}: {buttons:?}"
            );
        }
    }

    #[test]
    fn panel_shows_status_commands_and_files() {
        let f = Fixture::new();
        let chrome = f.chrome(1600, 2000);
        let (items, _) = chrome.panel_items();
        let texts = texts(&items);
        let expected: [&str; 36] = [
            "SESSION",
            "[Disconnect]",
            "[Full screen]",
            "[Refresh]",
            "[Ctrl+Alt+Del]",
            "VAULT",
            "[Unlock vault]",
            "LENT PASSWORD",
            "[Ask user]",
            "[Type]",
            "[Forget]",
            "STATUS",
            "Hostname",
            "WS-01",
            "Signed in",
            "CORP\\alice",
            "Internal IP",
            "192.168.1.20",
            "External IP",
            "203.0.113.9",
            "DNS servers",
            "192.168.1.1",
            "8.8.8.8",
            "OS",
            "Windows 11 Pro (build",
            "26100)",
            "Disks",
            &format!("C:\\{}45%", " ".repeat(20)),
            "112.0 GiB / 250.0 GiB",
            "COMMANDS",
            "[Command prompt]",
            "[Network connections]",
            "[Remote desktop]",
            "FILES",
            "[Upload file...]",
            "[Download file...]",
        ];
        assert_eq!(texts, expected);
        // The disk's bar is 45% full.
        let bar = items.iter().find_map(|i| match i {
            Item::Bar { fraction, .. } => Some(*fraction),
            _ => None,
        });
        assert!((bar.unwrap() - 0.448).abs() < 0.001);
        // Every button can be clicked.
        for item in &items {
            if let Item::Button(b) = item {
                assert_eq!(
                    chrome.hit(centre(b.rect)),
                    Hit::Action(b.action.clone()),
                    "{}",
                    b.label
                );
            }
        }
    }

    #[test]
    fn buttons_are_disabled_without_access_or_while_busy() {
        let mut f = Fixture::new();
        f.details.capabilities = vec!["desktop".into()];
        let chrome = f.chrome(1600, 2000);
        let (items, _) = chrome.panel_items();
        let button = |label: &str| {
            items
                .iter()
                .find_map(|i| match i {
                    Item::Button(b) if b.label == label => Some(b.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(button("Command prompt").enabled);
        let upload = button("Upload file...");
        assert!(!upload.enabled);
        assert_eq!(chrome.hit(centre(upload.rect)), Hit::Nothing);
        assert!(texts(&items).contains(&"No file access here.".to_owned()));

        // Offline: nothing to launch or transfer.
        f.details.online = false;
        f.details.capabilities.push("file_transfer".into());
        let chrome = f.chrome(1600, 2000);
        let (items, _) = chrome.panel_items();
        assert!(items.iter().all(|i| !matches!(i, Item::Button(b)
            if b.enabled && matches!(b.action, Action::Launch(_) | Action::Upload | Action::Download))));

        // Busy: no second transfer.
        let f = Fixture::new();
        let mut chrome = f.chrome(1600, 2000);
        chrome.panel.busy = Some("Uploading a.zip: 40%");
        let (items, _) = chrome.panel_items();
        assert!(texts(&items).contains(&"Uploading a.zip: 40%".to_owned()));
        assert!(items
            .iter()
            .any(|i| matches!(i, Item::Button(b) if b.action == Action::Upload && !b.enabled)));
    }

    #[test]
    fn without_the_api_the_panel_says_how_to_get_it() {
        let f = Fixture::new();
        let mut chrome = f.chrome(1600, 2000);
        chrome.panel.api = false;
        chrome.panel.details = None;
        let (items, _) = chrome.panel_items();
        let texts = texts(&items).join(" ");
        assert!(texts.contains("Start the viewer from the TUI"), "{texts}");
        // The session's own buttons need no API; the rest do.
        let enabled: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                Item::Button(b) if b.enabled => Some(b.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            enabled,
            [
                "Disconnect",
                "Full screen",
                "Refresh",
                "Ctrl+Alt+Del",
                "Unlock vault",
                "Ask user",
                "Type",
                "Forget"
            ]
        );
    }

    #[test]
    fn a_short_window_scrolls_the_panel() {
        let f = Fixture::new();
        let chrome = f.chrome(1600, 400);
        let max = chrome.max_scroll();
        assert!(max > 0);
        // Scrolled to the bottom, the last button is on screen and works...
        let scrolled = Chrome {
            scroll: max,
            ..f.chrome(1600, 400)
        };
        let (items, _) = scrolled.panel_items();
        let download = items
            .iter()
            .find_map(|i| match i {
                Item::Button(b) if b.action == Action::Download => Some(b.rect),
                _ => None,
            })
            .unwrap();
        assert!(download.bottom() <= 400);
        assert_eq!(
            scrolled.hit(centre(download)),
            Hit::Action(Action::Download)
        );
        // ...and not before scrolling: it is below the window.
        assert_eq!(chrome.hit((1500.0, 399.0)), Hit::Nothing);
    }

    #[test]
    fn menus_list_choices_and_close_on_a_click_elsewhere() {
        let mut f = Fixture::new();
        let anchor = f.chrome(1600, 900).toolbar_buttons()[0].rect;
        f.overlay = Some(Overlay::Menu(display_menu(anchor, DisplayMode::Fill)));
        let chrome = f.chrome(1600, 900);
        let Some(Overlay::Menu(menu)) = chrome.overlay else {
            unreachable!()
        };
        assert_eq!(
            menu.items
                .iter()
                .map(|i| (i.label.as_str(), i.checked))
                .collect::<Vec<_>>(),
            [
                ("Scale: fit, keep shape", false),
                ("Stretch: fill, distort", false),
                ("Fill: fill, crop edges", true),
                ("Original size: 1:1 pixels", false),
            ]
        );
        let (bx, rows) = chrome.menu_rects(menu);
        assert_eq!(bx.y, anchor.bottom() + M.border());
        assert_eq!(
            chrome.hit(centre(rows[1])),
            Hit::Action(Action::SetDisplay(DisplayMode::Stretch))
        );
        assert_eq!(chrome.hit((800.0, 800.0)), Hit::Dismiss);

        let monitors = monitor_menu(anchor, &f.monitors, Some(1));
        assert_eq!(
            monitors.items[0].label,
            r"1: \\.\DISPLAY1 1920x1080 (primary)"
        );
        assert_eq!(monitors.items[1].action, Action::SelectMonitor(1));
        assert!(monitors.items[1].checked && !monitors.items[0].checked);
        assert!(monitor_menu(anchor, &[], None).items[0]
            .label
            .contains("waiting"));
    }

    #[test]
    fn the_frame_rate_menu_offers_auto_max_and_fixed_rates() {
        let menu = frame_rate_menu(Rect::default(), FrameRate::Fixed(60), Some(60));
        let items: Vec<(&str, bool)> = menu
            .items
            .iter()
            .map(|i| (i.label.as_str(), i.checked))
            .collect();
        assert_eq!(
            items,
            [
                ("Auto: by network speed", false),
                ("Max: every screen change", false),
                ("120 fps", false),
                ("60 fps", true),
                ("30 fps", false),
                ("15 fps", false),
            ]
        );
        assert_eq!(menu.items[1].action, Action::SetFrameRate(FrameRate::Max));
        let auto = frame_rate_menu(Rect::default(), FrameRate::Auto, Some(30));
        assert_eq!(auto.items[0].label, "Auto: by network speed (now 30)");
        assert_eq!(frame_rate_label(FrameRate::Auto, None), "FPS: Auto");
        assert_eq!(frame_rate_label(FrameRate::Max, Some(240)), "FPS: Max");
        assert_eq!(frame_rate_label(FrameRate::Fixed(15), Some(15)), "FPS: 15");
    }

    #[test]
    fn dialogs_have_working_buttons() {
        let mut f = Fixture::new();
        f.overlay = Some(Overlay::Prompt(Prompt {
            title: "Download which file?".into(),
            text: r"C:\Users\".into(),
            purpose: PromptPurpose::Download,
            masked: false,
        }));
        let chrome = f.chrome(1600, 900);
        let g = chrome.overlay_geometry(chrome.overlay.unwrap()).unwrap();
        assert!(g.field.is_some());
        let labels: Vec<&str> = g.buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["OK", "Cancel"]);
        assert_eq!(
            chrome.hit(centre(g.buttons[0].rect)),
            Hit::Action(Action::PromptOk)
        );
        // Nothing else reacts while it is open.
        assert_eq!(chrome.hit((5.0, 5.0)), Hit::Nothing);

        // Everything draws without going out of bounds.
        let mut pixels = vec![0; 1600 * 900];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            width: 1600,
            height: 900,
        };
        let notices = [Notice {
            text: "Uploaded a.zip (1.0 MiB) to C:\\Users\\Public\\Desktop\\a.zip".into(),
            error: false,
            until: Instant::now(),
        }];
        Chrome {
            notices: &notices,
            cursor: Some((10.0, 10.0)),
            ..f.chrome(1600, 900)
        }
        .draw(&mut canvas);
        assert!(pixels.contains(&colors::TOOLBAR) && pixels.contains(&colors::NOTICE));
    }

    fn vault_rows(n: usize) -> Vec<VaultRow> {
        (0..n)
            .map(|i| VaultRow {
                name: format!("Contoso server {i}"),
                detail: "CORP\\admin".into(),
                username: true,
                password: i != 1,
                totp: i == 2,
            })
            .collect()
    }

    /// The panel's items from the VAULT heading up to the next one.
    fn vault_section(chrome: &Chrome) -> Vec<Item> {
        let (items, _) = chrome.panel_items();
        let heading = |i: &Item, s: &str| matches!(i, Item::Text { text, .. } if text == s);
        items
            .into_iter()
            .skip_while(|i| !heading(i, "VAULT"))
            .skip(1)
            .take_while(|i| !heading(i, "LENT PASSWORD"))
            .collect()
    }

    fn button(items: &[Item], label: &str) -> Button {
        items
            .iter()
            .find_map(|i| match i {
                Item::Button(b) if b.label == label => Some(b.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {label} button"))
    }

    #[test]
    fn a_locked_vault_offers_only_to_unlock() {
        let mut f = Fixture::new();
        let chrome = f.chrome(1600, 2000);
        let section = vault_section(&chrome);
        assert_eq!(texts(&section), ["[Unlock vault]"]);
        assert_eq!(
            chrome.hit(centre(button(&section, "Unlock vault").rect)),
            Hit::Action(Action::VaultUnlock)
        );
        // Why it could not be unlocked shows under the button; while it is
        // being checked the button waits.
        f.vault.note = Some(("Wrong master password.".into(), true));
        assert_eq!(
            texts(&vault_section(&f.chrome(1600, 2000))),
            ["[Unlock vault]", "Wrong master password."]
        );
        f.vault.busy = Some("Unlocking...".into());
        let section = vault_section(&f.chrome(1600, 2000));
        assert!(!button(&section, "Unlock vault").enabled);
        assert_eq!(texts(&section), ["[Unlock vault]", "Unlocking..."]);
    }

    #[test]
    fn an_unlocked_vault_searches_lists_and_offers_what_each_item_has() {
        let mut f = Fixture::new();
        f.vault.unlocked = true;
        f.vault.query = "contoso".into();
        let chrome = f.chrome(1600, 2000);
        let section = vault_section(&chrome);
        assert_eq!(
            texts(&section),
            [
                "<contoso>",
                "[Username]",
                "[Password]",
                "[TOTP]",
                "[Sync]",
                "[Lock]",
                "Click the field, type a",
                "name and press Enter."
            ]
        );
        // Nothing found yet: nothing to type. The field takes the keyboard.
        assert!(["Username", "Password", "TOTP"]
            .iter()
            .all(|l| !button(&section, l).enabled));
        let field = section
            .iter()
            .find_map(|i| match i {
                Item::Field { rect, .. } => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert_eq!(chrome.hit(centre(field)), Hit::Action(Action::VaultFocus));

        f.vault.set_rows(vault_rows(20));
        f.vault.select(1);
        let chrome = f.chrome(1600, 2000);
        let section = vault_section(&chrome);
        let panel = chrome.layout.panel.unwrap();
        let rows: Vec<(usize, Rect, bool)> = section
            .iter()
            .filter_map(|i| match i {
                Item::Row {
                    rect,
                    index,
                    selected,
                    ..
                } => Some((*index, *rect, *selected)),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), VAULT_ROWS);
        assert!(rows.iter().all(|(_, r, _)| panel.contains(centre(*r))));
        assert!(rows.iter().all(|(i, _, selected)| *selected == (*i == 1)));
        assert_eq!(
            chrome.hit(centre(rows[2].1)),
            Hit::Action(Action::VaultSelect(2))
        );
        assert!(chrome.over_vault_rows(centre(rows[0].1)));
        assert!(!chrome.over_vault_rows(centre(field)));
        assert!(texts(&section).contains(&"20 found: scroll for more.".to_owned()));
        // Item 1 has a username but no password or code; the three sit
        // side by side inside the panel.
        let kinds = ["Username", "Password", "TOTP"].map(|l| button(&section, l));
        assert_eq!(kinds.clone().map(|b| b.enabled), [true, false, false]);
        assert!(kinds.iter().all(|b| b.rect.y == kinds[0].rect.y));
        assert!(kinds.iter().all(|b| b.rect.right() <= panel.right()));
        assert_eq!(
            chrome.hit(centre(kinds[0].rect)),
            Hit::Action(Action::VaultType(TextKind::Username))
        );
        assert_eq!(chrome.hit(centre(kinds[1].rect)), Hit::Nothing);

        // The list follows the selection.
        f.vault.select(10);
        let section = vault_section(&f.chrome(1600, 2000));
        assert!(section.iter().any(|i| matches!(
            i,
            Item::Row {
                index: 10,
                selected: true,
                ..
            }
        )));

        // Busy: nothing can be typed, synced or locked meanwhile.
        f.vault.select(2);
        f.vault.busy = Some("Searching...".into());
        f.vault.focused = true;
        let chrome = f.chrome(1600, 2000);
        let section = vault_section(&chrome);
        assert!(section
            .iter()
            .all(|i| !matches!(i, Item::Button(b) if b.enabled)));

        let mut pixels = vec![0; 1600 * 2000];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            width: 1600,
            height: 2000,
        };
        chrome.draw(&mut canvas);
        assert!(pixels.contains(&colors::ACTIVE) && pixels.contains(&colors::FIELD));
    }

    #[test]
    fn the_vault_selection_scrolls_into_view() {
        let mut v = Vault::default();
        v.move_by(1);
        assert_eq!((v.selected, v.top), (0, 0));
        v.set_rows(vault_rows(20));
        v.move_by(VAULT_ROWS as isize);
        assert_eq!((v.selected, v.top), (VAULT_ROWS, 1));
        v.move_by(100);
        assert_eq!((v.selected, v.top), (19, 20 - VAULT_ROWS));
        v.move_by(-100);
        assert_eq!((v.selected, v.top), (0, 0));
        v.select(5);
        v.set_rows(vault_rows(2));
        assert_eq!((v.selected, v.top), (0, 0));
    }

    #[test]
    fn a_masked_prompt_never_draws_what_was_typed() {
        let mut f = Fixture::new();
        let draw = |f: &Fixture| {
            let mut pixels = vec![0; 1600 * 900];
            let mut canvas = Canvas {
                pixels: &mut pixels,
                width: 1600,
                height: 900,
            };
            f.chrome(1600, 900).draw(&mut canvas);
            pixels
        };
        let prompt = |text: &str, masked| {
            Some(Overlay::Prompt(Prompt {
                title: "Master password".into(),
                text: text.into(),
                purpose: PromptPurpose::VaultUnlock,
                masked,
            }))
        };
        f.overlay = prompt("hunter2", true);
        let masked = draw(&f);
        f.overlay = prompt("*******", false);
        assert!(masked == draw(&f));
        f.overlay = prompt("hunter2", false);
        assert!(masked != draw(&f));
    }

    #[test]
    fn tiny_windows_draw_without_panicking() {
        let mut f = Fixture::new();
        f.vault.unlocked = true;
        f.vault.set_rows(vault_rows(3));
        for (w, h) in [(1, 1), (40, 30), (300, 60), (900, 200)] {
            let mut pixels = vec![0; (w * h) as usize];
            let mut canvas = Canvas {
                pixels: &mut pixels,
                width: w,
                height: h,
            };
            f.chrome(w, h).draw(&mut canvas);
        }
    }

    #[test]
    fn quick_commands_parse() {
        let c: QuickCommand = "Network connections=ncpa.cpl".parse().unwrap();
        assert_eq!(
            (c.label.as_str(), c.command.as_str()),
            ("Network connections", "ncpa.cpl")
        );
        let c: QuickCommand = "RDP to server=mstsc /v:srv=01".parse().unwrap();
        assert_eq!(c.command, "mstsc /v:srv=01");
        let c: QuickCommand = "cmd".parse().unwrap();
        assert_eq!(c.label, "cmd");
        assert!("Nothing=".parse::<QuickCommand>().is_err());
    }

    #[test]
    fn text_helpers() {
        assert_eq!(truncate("Network connections", 10), "Network ..");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(
            wrap("Windows 11 Pro (build 26100)", 16),
            ["Windows 11 Pro", "(build 26100)"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
        assert_eq!(wrap("abcd efgh", 4), ["abcd", "efgh"]);
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(112 << 30), "112.0 GiB");
    }
}
