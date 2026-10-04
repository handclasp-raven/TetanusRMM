//! The viewer's controls around the remote picture: a header along the
//! top (the machine's name, display mode, frame rate, monitor, text size,
//! and the session's buttons), a rail of tabs at the right that opens one
//! panel at a time beside it (the technician's password vault and the
//! lent password; the agent's status; command buttons and file transfer),
//! a footer saying how the session is going, plus drop-down menus, a
//! one-line text prompt, a confirmation box and notices.
//!
//! Everything here is layout and drawing. A [`Chrome`] describes what is on
//! screen; the same geometry serves drawing ([`Chrome::draw`]) and clicks
//! ([`Chrome::hit`]), so the two cannot disagree. The window code in
//! `main.rs` owns the state and acts on the [`Action`]s.
//!
//! The sizes are the design's (`branding/viewer`), which is drawn with
//! text 12 pixels high; they grow and shrink with the text size.

use std::path::PathBuf;
use std::time::Instant;

use brand::icons::Icon;
use protocol::media::{FrameRate, MonitorInfo};
use protocol::vault::TextKind;

use crate::api::AgentDetails;
use crate::icons;
use crate::render::{self, Canvas, DisplayMode, Glyphs, Rect, Style};
use crate::theme::Theme;

/// Text size (as the Text menu counts it, at a scale factor of 1) unless
/// the technician picks another.
pub const DEFAULT_FONT_PX: u32 = 9;
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
/// Longest machine name the header shows, in characters.
const HOST_CHARS: usize = 24;

/// Sizes in physical pixels, from the text size and the window's scale
/// factor. Everything follows the text size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    /// How high body text is (its em).
    pub font: u32,
}

impl Metrics {
    /// Text of size `font_px` (clamped to [`MIN_FONT_PX`]..=[`MAX_FONT_PX`])
    /// at a scale factor of 1. A size counts capitals, roughly: the
    /// default 9 is the design's 12 pixel text.
    pub fn new(font_px: u32, factor: f64) -> Self {
        let size = f64::from(font_px.clamp(MIN_FONT_PX, MAX_FONT_PX));
        Self {
            font: ((size * 4.0 / 3.0 * factor.max(1.0)).round() as u32).max(1),
        }
    }

    /// The default text size.
    pub fn for_scale_factor(factor: f64) -> Self {
        Self::new(DEFAULT_FONT_PX, factor)
    }

    /// A length of the design's, which is drawn with 12 pixel text.
    pub fn px(&self, design: u32) -> u32 {
        ((design * self.font + 6) / 12).max(1)
    }

    /// Buttons, values in the header, what is typed.
    pub fn body(&self) -> Style {
        Style::new(self.font)
    }

    /// The machine's name.
    pub fn strong(&self) -> Style {
        self.body().semibold()
    }

    /// A panel's title.
    pub fn heading(&self) -> Style {
        self.strong().tracked(self.px(2))
    }

    /// A heading inside a panel.
    pub fn subheading(&self) -> Style {
        self.label().semibold().tracked(self.px(2))
    }

    /// What a value is, hints, the footer.
    pub fn label(&self) -> Style {
        Style::new(self.px(11))
    }

    /// The agent's details.
    pub fn value(&self) -> Style {
        Style::new(self.px(13))
    }

    /// A tab's name under its icon.
    pub fn tab_label(&self) -> Style {
        Style::new(self.px(9)).tracked(self.px(1))
    }

    /// Width of a character of body text.
    pub fn char_w(&self) -> u32 {
        self.body().advance()
    }

    /// From one line of `style` to the next.
    pub fn line_h(&self, style: Style) -> u32 {
        style.px * 3 / 2
    }

    pub fn button_h(&self) -> u32 {
        self.px(32)
    }

    pub fn select_h(&self) -> u32 {
        self.px(26)
    }

    pub fn field_h(&self) -> u32 {
        self.px(36)
    }

    pub fn header_h(&self) -> u32 {
        self.px(48)
    }

    pub fn footer_h(&self) -> u32 {
        self.px(28)
    }

    /// A tab is this wide and high, and so the rail this wide.
    pub fn tab(&self) -> u32 {
        self.px(56)
    }

    pub fn panel_w(&self) -> u32 {
        self.px(320)
    }

    /// Between the panel's edge and what is in it.
    pub fn panel_pad(&self) -> u32 {
        self.px(20)
    }

    /// Between a button's edge and its label.
    pub fn pad(&self) -> u32 {
        self.px(10)
    }

    /// Between buttons.
    pub fn gap(&self) -> u32 {
        self.px(6)
    }

    pub fn icon(&self) -> u32 {
        self.px(14)
    }

    pub fn tab_icon(&self) -> u32 {
        self.px(18)
    }

    /// A disk usage bar's height.
    pub fn bar_h(&self) -> u32 {
        self.px(6)
    }

    pub fn radius(&self) -> u32 {
        self.px(4)
    }

    pub fn border(&self) -> u32 {
        self.px(1)
    }

    /// Where text of `style` goes in `rect` to sit centred vertically.
    pub fn text_y(&self, rect: Rect, style: Style) -> u32 {
        rect.y + rect.h.saturating_sub(style.px) / 2
    }
}

/// A panel the rail opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    /// The technician's vault, and the password the user lends.
    Vault,
    /// What the agent says about its machine.
    Status,
    /// Command buttons and file transfer.
    Tools,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Vault, Tab::Status, Tab::Tools];

    /// Under its icon on the rail.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Vault => "VAULT",
            Tab::Status => "STATUS",
            Tab::Tools => "TOOLS",
        }
    }

    /// At the top of its panel.
    pub fn title(self) -> &'static str {
        match self {
            Tab::Vault => "VAULT",
            Tab::Status => "STATUS",
            Tab::Tools => "COMMANDS + FILES",
        }
    }

    /// As the settings file names it.
    pub fn key(self) -> &'static str {
        match self {
            Tab::Vault => "vault",
            Tab::Status => "status",
            Tab::Tools => "tools",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Tab::Vault => icons::KEY,
            Tab::Status => icons::INFO,
            Tab::Tools => icons::TERMINAL,
        }
    }
}

/// Where the header, the rail, the panel, the footer and the remote
/// picture go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub window: Rect,
    pub header: Rect,
    pub footer: Rect,
    /// The tabs, down the right edge.
    pub rail: Rect,
    /// Left of the rail. `None` with no tab open, or when the window is
    /// too narrow for it.
    pub panel: Option<Rect>,
    pub desktop: Rect,
}

pub fn layout(width: u32, height: u32, m: &Metrics, open: bool) -> Layout {
    let window = Rect::new(0, 0, width, height);
    let header_h = m.header_h().min(height);
    let footer_h = m.footer_h().min(height - header_h);
    let body_h = height - header_h - footer_h;
    let rail_w = m.tab().min(width);
    let rest = width - rail_w;
    // Leave the picture at least as much room as the panel takes.
    let panel_w = if open && rest >= 2 * m.panel_w() {
        m.panel_w()
    } else {
        0
    };
    Layout {
        window,
        header: Rect::new(0, 0, width, header_h),
        footer: Rect::new(0, height - footer_h, width, footer_h),
        rail: Rect::new(rest, header_h, rail_w, body_h),
        panel: (panel_w > 0).then(|| Rect::new(rest - panel_w, header_h, panel_w, body_h)),
        desktop: Rect::new(0, header_h, rest - panel_w, body_h),
    }
}

/// A button on the viewer's own controls: in the header, on the rail or
/// in the panel, or an item of a menu or dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    DisplayMenu,
    FrameRateMenu,
    SetFrameRate(FrameRate),
    TextMenu,
    /// Text of this size (at a scale factor of 1).
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
    /// Open this tab's panel, or close it if it is the one open.
    Tab(Tab),
    /// Close the panel, or open the tab that was open last.
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

/// How a button is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Normal,
    /// Ends the session.
    Danger,
    /// Opens a menu: shows the current choice, with a drop-down marker.
    Select,
    /// A tab on the rail: its icon over its name.
    Tab,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub rect: Rect,
    /// Nothing, for a button that is only its icon.
    pub label: String,
    pub action: Action,
    pub enabled: bool,
    /// Shown switched on (full screen; the open tab).
    pub active: bool,
    pub kind: Kind,
    pub icon: Option<Icon>,
    /// What a select box chooses, written to its left.
    pub caption: String,
}

impl Button {
    fn new(rect: Rect, label: impl Into<String>, action: Action) -> Self {
        Self {
            rect,
            label: label.into(),
            action,
            enabled: true,
            active: false,
            kind: Kind::Normal,
            icon: None,
            caption: String::new(),
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

/// The header's state.
#[derive(Debug, Clone, Copy)]
pub struct Toolbar<'a> {
    /// The machine's name (or what stands in for it until it is known).
    pub host: &'a str,
    pub display: DisplayMode,
    /// The technician's frame-rate choice.
    pub frame_rate: FrameRate,
    /// What the agent streams at now, once it has said (for Auto, the rate
    /// it picked).
    pub streamed_fps: Option<u32>,
    pub monitors: &'a [MonitorInfo],
    pub active_monitor: Option<u32>,
    /// Text size, as chosen (before the scale factor).
    pub font_px: u32,
    pub fullscreen: bool,
}

/// What the footer says about the session.
#[derive(Debug, Clone, Copy)]
pub struct Footer<'a> {
    /// How the session is carried ("encrypted · direct"), or why there is
    /// none ("Connecting...").
    pub link: &'a str,
    /// Whether there is a session.
    pub live: bool,
    /// Pictures shown a second.
    pub fps: u32,
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
    /// Whether the agent holds a password the user lent, as far as this
    /// viewer has heard.
    pub lent: Option<bool>,
}

/// Something drawn in the panel.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Text {
        x: u32,
        y: u32,
        style: Style,
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
    /// A hairline between two parts of a panel.
    Rule {
        rect: Rect,
    },
    /// A box set into the panel, behind what follows it (a disk).
    Card {
        rect: Rect,
    },
    /// Where files can be dropped to upload them.
    DropZone {
        rect: Rect,
    },
}

impl Item {
    /// Where it is, if it is a box (text has only a top-left corner).
    fn rect(&self) -> Option<Rect> {
        match self {
            Item::Text { .. } => None,
            Item::Bar { rect, .. }
            | Item::Field { rect, .. }
            | Item::Row { rect, .. }
            | Item::Rule { rect }
            | Item::Card { rect }
            | Item::DropZone { rect } => Some(*rect),
            Item::Button(b) => Some(b.rect),
        }
    }

    /// Move it up by `by` pixels (past the top wraps around, which
    /// [`Chrome::visible`] takes for out of sight).
    fn scroll(&mut self, by: u32) {
        match self {
            Item::Text { y, .. } => *y = y.wrapping_sub(by),
            Item::Bar { rect, .. }
            | Item::Field { rect, .. }
            | Item::Row { rect, .. }
            | Item::Rule { rect }
            | Item::Card { rect }
            | Item::DropZone { rect } => rect.y = rect.y.wrapping_sub(by),
            Item::Button(b) => b.rect.y = b.rect.y.wrapping_sub(by),
        }
    }

    fn top(&self) -> u32 {
        match self {
            Item::Text { y, .. } => *y,
            _ => self.rect().map_or(0, |r| r.y),
        }
    }

    fn bottom(&self) -> u32 {
        match self {
            Item::Text { y, style, .. } => y.saturating_add(style.px),
            _ => self.rect().map_or(0, |r| r.y.saturating_add(r.h)),
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

/// A piece of the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterItem {
    pub x: u32,
    pub text: String,
    pub color: u32,
    /// A round dot of the same colour before the text.
    pub dot: bool,
}

/// Everything the viewer draws besides the remote picture.
#[derive(Clone, Copy)]
pub struct Chrome<'a> {
    pub metrics: Metrics,
    pub layout: Layout,
    pub theme: &'a Theme,
    pub glyphs: &'a Glyphs,
    /// The tab whose panel is open.
    pub tab: Option<Tab>,
    pub toolbar: Toolbar<'a>,
    pub footer: Footer<'a>,
    pub panel: Panel<'a>,
    /// How far the panel is scrolled, in pixels.
    pub scroll: u32,
    pub overlay: Option<&'a Overlay>,
    /// The vault tab's state.
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

/// The FPS box's text: the choice, and for Auto what it came to.
pub fn frame_rate_label(choice: FrameRate, streamed: Option<u32>) -> String {
    match (choice, streamed) {
        (FrameRate::Auto, Some(fps)) => format!("Auto ({fps})"),
        _ => choice.to_string(),
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

/// The tab remembered in `path` (see [`save_setting`]): `Some(None)` if
/// the panel was closed, `None` if nothing is remembered.
pub fn load_panel(path: &std::path::Path) -> Option<Option<Tab>> {
    let text = std::fs::read_to_string(path).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&text).ok()?;
    let key = settings.get("panel")?.as_str()?;
    if key == PANEL_CLOSED {
        return Some(None);
    }
    Tab::ALL.into_iter().find(|t| t.key() == key).map(Some)
}

/// What the settings file says for no tab open.
pub const PANEL_CLOSED: &str = "none";

/// An operating system's name cut down for the footer: `Windows 11 IoT
/// Enterprise LTSC 2024 (build 26100)` becomes `Win 11 IoT LTSC 2024`.
pub fn short_os(os: &str) -> String {
    let name = os.split(" (").next().unwrap_or(os);
    name.split_whitespace()
        .filter(|word| *word != "Enterprise")
        .map(|word| if word == "Windows" { "Win" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
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

/// How full a disk is, 0-1.
fn used_fraction(disk: &crate::api::Disk) -> f64 {
    if disk.total_bytes == 0 {
        0.0
    } else {
        disk.used_bytes as f64 / disk.total_bytes as f64
    }
}

/// A panel's contents, laid out from the top down.
struct Flow<'a> {
    m: Metrics,
    theme: &'a Theme,
    /// The left edge and the width of what goes in the panel.
    x: u32,
    w: u32,
    /// Where the next thing goes.
    y: u32,
    items: Vec<Item>,
}

impl Flow<'_> {
    /// Leave a gap (a length of the design's).
    fn skip(&mut self, design: u32) {
        self.y += self.m.px(design);
    }

    /// `s` from `(x, y)` in lines at most `width` pixels wide. Where it
    /// ends.
    fn text_at(
        &mut self,
        (x, mut y): (u32, u32),
        width: u32,
        style: Style,
        color: u32,
        s: &str,
    ) -> u32 {
        let chars = (width / style.advance()).max(1) as usize;
        for (i, line) in wrap(s, chars).into_iter().enumerate() {
            if i > 0 {
                y += self.m.line_h(style) - style.px;
            }
            self.items.push(Item::Text {
                x,
                y,
                style,
                color,
                text: line,
            });
            y += style.px;
        }
        y
    }

    /// `s` across the panel.
    fn text(&mut self, style: Style, color: u32, s: &str) {
        self.y = self.text_at((self.x, self.y), self.w, style, color, s);
    }

    /// What a value is, with the value (or values) under it, `width` wide
    /// from `x`. Where it ends; the flow does not move.
    fn field_at(&mut self, x: u32, width: u32, label: &str, values: &[String]) -> u32 {
        let (m, t) = (self.m, self.theme);
        let mut y = self.text_at((x, self.y), width, m.label(), t.muted, label);
        for value in values {
            y = self.text_at((x, y + m.px(3)), width, m.value(), t.text, value);
        }
        y
    }

    /// The same across the panel, with the gap that follows a field.
    fn field(&mut self, label: &str, values: &[String]) {
        self.y = self.field_at(self.x, self.w, label, values);
        self.skip(14);
    }

    /// A button in `rect`, its label cut to fit.
    fn fitted(
        &self,
        rect: Rect,
        label: &str,
        icon: Option<Icon>,
        action: Action,
        enabled: bool,
    ) -> Button {
        let m = self.m;
        let taken = 2 * m.pad() + icon.map_or(0, |_| m.icon() + m.px(8));
        let room = (rect.w.saturating_sub(taken) / m.char_w()) as usize;
        Button {
            enabled,
            icon,
            ..Button::new(rect, truncate(label, room), action)
        }
    }

    /// A button across the panel.
    fn button(&mut self, label: &str, icon: Option<Icon>, action: Action, enabled: bool) {
        let rect = Rect::new(self.x, self.y, self.w, self.m.button_h());
        let button = self.fitted(rect, label, icon, action, enabled);
        self.items.push(Item::Button(button));
        self.y += self.m.button_h();
    }

    /// Buttons side by side, sharing the panel's width equally.
    fn row(&mut self, buttons: &[(&str, Option<Icon>, Action, bool)]) {
        let m = self.m;
        let n = buttons.len() as u32;
        let w = self.w.saturating_sub((n - 1) * m.gap()) / n;
        for (i, (label, icon, action, enabled)) in buttons.iter().enumerate() {
            let rect = Rect::new(self.x + i as u32 * (w + m.gap()), self.y, w, m.button_h());
            let button = self.fitted(rect, label, *icon, action.clone(), *enabled);
            self.items.push(Item::Button(button));
        }
        self.y += m.button_h();
    }
}

/// Where the header's parts go.
struct Header {
    mark: Rect,
    /// Where the machine's name starts, and the name as shown.
    host: (u32, String),
    /// The hairline between the name and the select boxes.
    divider: Rect,
    buttons: Vec<Button>,
}

impl Chrome<'_> {
    fn m(&self) -> &Metrics {
        &self.metrics
    }

    // --- header -------------------------------------------------------------

    fn header(&self) -> Header {
        let m = *self.m();
        let t = &self.toolbar;
        let h = self.layout.header;
        let body = m.body();
        let edge = m.px(16);
        // Centred in the header's height.
        let down = |height: u32| h.y + h.h.saturating_sub(height) / 2;

        let mark = Rect::new(h.x + edge, down(m.px(18)), m.px(18), m.px(18));
        let host = truncate(t.host, HOST_CHARS);
        let host_x = mark.right() + m.px(10);
        let mut x = host_x + render::text_width(&host, m.strong()) + m.px(18);
        let divider = Rect::new(x, down(m.select_h()), m.border(), m.select_h());
        x += m.px(18);

        let monitor = match t
            .active_monitor
            .and_then(|a| t.monitors.iter().position(|mon| mon.id == a))
        {
            Some(i) => format!("{} of {}", i + 1, t.monitors.len()),
            None => "-".to_owned(),
        };
        let selects = [
            ("Display", t.display.label().to_owned(), Action::DisplayMenu),
            (
                "FPS",
                frame_rate_label(t.frame_rate, t.streamed_fps),
                Action::FrameRateMenu,
            ),
            ("Monitor", monitor, Action::MonitorMenu),
            ("Text", t.font_px.to_string(), Action::TextMenu),
        ];
        let mut buttons = Vec::new();
        for (caption, value, action) in selects {
            let inside = m.px(6);
            x += render::text_width(caption, body) + inside;
            // The value, then the drop-down marker.
            let w = inside + render::text_width(&value, body) + m.px(8) + m.px(8) + inside;
            let rect = Rect::new(x, down(m.select_h()), w, m.select_h());
            x = rect.right() + m.px(12);
            buttons.push(Button {
                kind: Kind::Select,
                caption: caption.to_owned(),
                ..Button::new(rect, value, action)
            });
        }

        // The session's buttons, from the right edge inwards.
        let mut right = h.right().saturating_sub(edge);
        let mut session = Vec::new();
        let icon_w = m.icon() + m.px(8);
        let mut add = |label: &str, icon: Option<Icon>, action: Action| {
            let w = if label.is_empty() {
                m.button_h()
            } else {
                2 * m.pad() + icon.map_or(0, |_| icon_w) + render::text_width(label, body)
            };
            let rect = Rect::new(right.saturating_sub(w), down(m.button_h()), w, m.button_h());
            right = rect.x.saturating_sub(m.gap());
            session.push(Button {
                icon,
                ..Button::new(rect, label, action)
            });
        };
        add("Disconnect", Some(icons::POWER), Action::Disconnect);
        add("Ctrl+Alt+Del", None, Action::SecureAttention);
        add("", Some(icons::FULLSCREEN), Action::ToggleFullscreen);
        add("", Some(icons::REFRESH), Action::Keyframe);
        session[0].kind = Kind::Danger;
        session[2].active = t.fullscreen;
        // A narrow window: leave out select boxes from the end (text size
        // has shortcuts, monitors too) rather than run under the session's
        // buttons. The first two always stay.
        let room = right.saturating_sub(m.gap());
        while buttons.len() > 2 && buttons.last().is_some_and(|b| b.rect.right() > room) {
            buttons.pop();
        }
        buttons.extend(session.into_iter().rev());
        Header {
            mark,
            host: (host_x, host),
            divider,
            buttons,
        }
    }

    /// The header's select boxes, then the session's buttons.
    pub fn toolbar_buttons(&self) -> Vec<Button> {
        self.header().buttons
    }

    // --- rail ---------------------------------------------------------------

    /// The tabs, top down: as many as the rail is high enough for.
    pub fn rail_buttons(&self) -> Vec<Button> {
        let rail = self.layout.rail;
        let side = self.m().tab();
        Tab::ALL
            .into_iter()
            .enumerate()
            .map(|(i, tab)| Button {
                kind: Kind::Tab,
                icon: Some(tab.icon()),
                active: self.tab == Some(tab),
                ..Button::new(
                    Rect::new(rail.x, rail.y + i as u32 * side, rail.w, side),
                    tab.label(),
                    Action::Tab(tab),
                )
            })
            .filter(|b| b.rect.bottom() <= rail.bottom())
            .collect()
    }

    // --- footer -------------------------------------------------------------

    /// What the footer says, left to right: how the session is carried and
    /// how fast, who is signed in, the machine's address and system, and
    /// at the right how full its first disk is. What does not fit is left
    /// out, from the end.
    pub fn footer_items(&self) -> Vec<FooterItem> {
        let m = *self.m();
        let (t, f) = (self.theme, self.layout.footer);
        let style = m.label();
        let (edge, gap) = (m.px(16), m.px(18));
        let details = self.panel.details;
        let mut left = vec![(
            self.footer.link.to_owned(),
            if self.footer.live { t.ok_text } else { t.muted },
            self.footer.live,
        )];
        if self.footer.live {
            left.push((format!("{} fps", self.footer.fps), t.muted, false));
        }
        if let Some(d) = details {
            let user = d.logged_in_users.as_ref().and_then(|users| users.first());
            let os = d.os.as_deref().map(short_os);
            for text in [user.cloned(), d.local_ip.clone(), os]
                .into_iter()
                .flatten()
            {
                left.push((text, t.muted, false));
            }
        }
        let disk = details
            .and_then(|d| d.disks.as_ref()?.first())
            .map(|disk| format!("{} {:.0}%", disk.name, used_fraction(disk) * 100.0));
        let disk_x = disk.as_ref().map(|text| {
            f.right()
                .saturating_sub(edge + render::text_width(text, style))
        });
        let limit = disk_x.map_or(f.right().saturating_sub(edge), |x| x.saturating_sub(gap));

        let mut items = Vec::new();
        let mut x = f.x + edge;
        for (i, (text, color, dot)) in left.into_iter().enumerate() {
            let w = render::text_width(&text, style) + if dot { m.px(12) } else { 0 };
            if i > 0 && x + w > limit {
                break;
            }
            items.push(FooterItem {
                x,
                text,
                color,
                dot,
            });
            x += w + gap;
        }
        if let (Some(text), Some(disk_x)) = (disk, disk_x) {
            if disk_x + gap >= x {
                items.push(FooterItem {
                    x: disk_x,
                    text,
                    color: t.muted,
                    dot: false,
                });
            }
        }
        items
    }

    // --- panel --------------------------------------------------------------

    /// The open tab's contents in window coordinates (scrolled), and the
    /// height of all of it.
    pub fn panel_items(&self) -> (Vec<Item>, u32) {
        let (Some(panel), Some(tab)) = (self.layout.panel, self.tab) else {
            return (Vec::new(), 0);
        };
        let m = *self.m();
        let pad = m.panel_pad();
        let mut flow = Flow {
            m,
            theme: self.theme,
            x: panel.x + pad,
            w: panel.w - 2 * pad,
            y: panel.y + pad,
            items: Vec::new(),
        };
        flow.text(m.heading(), self.theme.heading, tab.title());
        flow.skip(16);
        match tab {
            Tab::Vault => self.vault_items(&mut flow),
            Tab::Status => self.status_items(&mut flow),
            Tab::Tools => self.tools_items(&mut flow),
        }
        let height = flow.y - panel.y + pad;
        for item in &mut flow.items {
            item.scroll(self.scroll);
        }
        (flow.items, height)
    }

    /// The vault: locked, an unlock button; unlocked, a search field, the
    /// items found and what to type. Then the password the remote user
    /// lends.
    fn vault_items(&self, f: &mut Flow) {
        let (m, t) = (f.m, self.theme);
        let v = self.vault;
        let idle = v.busy.is_none();
        if v.unlocked {
            f.items.push(Item::Field {
                rect: Rect::new(f.x, f.y, f.w, m.field_h()),
                text: v.query.clone(),
                focused: v.focused,
            });
            f.y += m.field_h();
            f.skip(8);
            let row_h = m.px(40);
            let room = f.w.saturating_sub(2 * m.pad());
            for index in (v.top..v.rows.len()).take(VAULT_ROWS) {
                let found = &v.rows[index];
                f.items.push(Item::Row {
                    rect: Rect::new(f.x, f.y, f.w, row_h),
                    index,
                    name: truncate(&found.name, (room / m.body().advance()) as usize),
                    detail: truncate(&found.detail, (room / m.label().advance()) as usize),
                    selected: index == v.selected,
                });
                f.y += row_h;
            }
            if !v.rows.is_empty() {
                f.skip(8);
            }
            let has = |kind| idle && v.current().is_some_and(|found| found.has(kind));
            let kinds = [
                ("Type username", icons::USER, TextKind::Username),
                ("Type password", icons::KEY, TextKind::Password),
                ("Type TOTP", icons::CLOCK, TextKind::Totp),
            ];
            for (label, icon, kind) in kinds {
                f.button(label, Some(icon), Action::VaultType(kind), has(kind));
                f.skip(6);
            }
            f.skip(2);
            f.row(&[
                ("Sync", None, Action::VaultSync, idle),
                ("Lock", Some(icons::LOCK), Action::VaultLock, idle),
            ]);
        } else {
            f.button("Unlock vault", Some(icons::LOCK), Action::VaultUnlock, idle);
        }
        let more = v.rows.len().saturating_sub(VAULT_ROWS);
        let note = match (&v.busy, &v.note) {
            (Some(busy), _) => Some((busy.clone(), t.muted)),
            (None, Some((note, true))) => Some((note.clone(), t.warn)),
            (None, Some((note, false))) => Some((note.clone(), t.muted)),
            (None, None) if more > 0 => {
                Some((format!("{} found: scroll for more.", v.rows.len()), t.muted))
            }
            (None, None) => None,
        };
        if let Some((note, color)) = note {
            f.skip(8);
            f.text(m.label(), color, &note);
        }

        f.skip(18);
        f.items.push(Item::Rule {
            rect: Rect::new(f.x, f.y, f.w, m.border()),
        });
        f.y += m.border();
        f.skip(18);

        // The password the remote user lends (see `protocol::credential`).
        // It is kept on their machine, and another technician may be the
        // one who asked for it or had it forgotten: what this viewer has
        // heard is shown, but every choice is always there, and the agent
        // says what came of it.
        f.items.push(Item::Text {
            x: f.x,
            y: f.y,
            style: m.subheading(),
            color: t.heading,
            text: "LENT PASSWORD".into(),
        });
        if let Some(lent) = self.panel.lent {
            let tag = if lent { "stored" } else { "none" };
            f.items.push(Item::Text {
                x: (f.x + f.w).saturating_sub(render::text_width(tag, m.label())),
                y: f.y,
                style: m.label(),
                color: t.muted,
                text: tag.into(),
            });
        }
        f.y += m.label().px;
        f.skip(8);
        f.text(
            m.label(),
            t.muted,
            "Ask the user to type a password you can use while they are away. You never see it.",
        );
        f.skip(8);
        f.row(&[
            ("Ask user", None, Action::PasswordRequest, true),
            ("Type", None, Action::PasswordType, true),
            ("Forget", None, Action::PasswordForget, true),
        ]);
    }

    /// What the agent says about its machine.
    fn status_items(&self, f: &mut Flow) {
        let (m, t) = (f.m, self.theme);
        let p = &self.panel;
        let d = match (p.api, p.details) {
            (false, _) => {
                return f.text(
                    m.label(),
                    t.muted,
                    "Start the viewer from the TUI to see the agent's status here and use the \
                     commands and file transfer.",
                );
            }
            (true, None) => return f.text(m.label(), t.muted, p.error.unwrap_or("Loading...")),
            (true, Some(d)) => d,
        };
        let or_unknown = |v: &Option<String>| vec![v.clone().unwrap_or_else(|| "-".into())];
        f.field("Hostname", &or_unknown(&d.hostname));
        f.field(
            "Signed in",
            &match &d.logged_in_users {
                None => vec!["-".into()],
                Some(users) if users.is_empty() => vec!["nobody".into()],
                Some(users) => users.clone(),
            },
        );
        // The addresses, two abreast.
        let column = f.w.saturating_sub(m.px(14)) / 2;
        let second = f.x + column + m.px(14);
        let internal = f.field_at(f.x, column, "Internal IP", &or_unknown(&d.local_ip));
        let external = f.field_at(second, column, "External IP", &or_unknown(&d.remote_ip));
        f.y = internal.max(external);
        f.skip(14);
        f.y = f.field_at(
            f.x,
            column,
            "DNS servers",
            &match &d.dns_servers {
                Some(list) if !list.is_empty() => list.clone(),
                Some(_) => vec!["none".into()],
                None => vec!["-".into()],
            },
        );
        f.skip(14);
        f.field("OS", &or_unknown(&d.os));
        match &d.disks {
            Some(disks) if !disks.is_empty() => {
                for disk in disks {
                    self.disk_card(f, disk);
                    f.skip(8);
                }
            }
            _ => f.field("Disks", &["-".into()]),
        }
        if !d.online {
            f.skip(6);
            f.text(m.label(), t.warn, "The agent is offline.");
        }
        if let Some(error) = p.error {
            f.skip(6);
            f.text(m.label(), t.warn, &format!("Not updating: {error}"));
        }
    }

    /// A disk: its name and how full it is, as a number, a bar and sizes.
    fn disk_card(&self, f: &mut Flow, disk: &crate::api::Disk) {
        let (m, t) = (f.m, self.theme);
        let inset = m.px(12);
        let (x, w) = (f.x + inset, f.w.saturating_sub(2 * inset));
        let fraction = used_fraction(disk);
        let percent = format!("{:.0}%", fraction * 100.0);
        let percent_w = render::text_width(&percent, m.value());
        let name_room = w.saturating_sub(percent_w + m.gap()) / m.label().advance();
        let head = m.value().px;
        let h = inset + head + m.px(6) + m.bar_h() + m.px(6) + m.label().px + inset;
        f.items.push(Item::Card {
            rect: Rect::new(f.x, f.y, f.w, h),
        });
        let mut y = f.y + inset;
        f.items.push(Item::Text {
            x,
            y: y + (head - m.label().px) / 2,
            style: m.label(),
            color: t.muted,
            text: truncate(&format!("Disk {}", disk.name), name_room as usize),
        });
        f.items.push(Item::Text {
            x: (x + w).saturating_sub(percent_w),
            y,
            style: m.value(),
            color: t.text,
            text: percent,
        });
        y += head + m.px(6);
        f.items.push(Item::Bar {
            rect: Rect::new(x, y, w, m.bar_h()),
            fraction,
        });
        y += m.bar_h() + m.px(6);
        f.items.push(Item::Text {
            x,
            y,
            style: m.label(),
            color: t.muted,
            text: format!(
                "{} of {} used",
                bytes(disk.used_bytes),
                bytes(disk.total_bytes)
            ),
        });
        f.y += h;
    }

    /// The command buttons and file transfer.
    fn tools_items(&self, f: &mut Flow) {
        let (m, t) = (f.m, self.theme);
        let p = &self.panel;
        f.text(m.label(), t.muted, "Commands");
        f.skip(8);
        let can_launch = p.details.is_some_and(|d| d.online && d.can("desktop"));
        if p.commands.is_empty() {
            f.text(m.label(), t.muted, "None: add some in the TUI.");
        }
        for (i, command) in p.commands.iter().enumerate() {
            if i > 0 {
                f.skip(6);
            }
            f.button(
                &command.label,
                Some(icons::for_command(&command.command)),
                Action::Launch(i),
                can_launch,
            );
        }

        f.skip(18);
        f.text(m.label(), t.muted, "Files");
        f.skip(8);
        let can_transfer = p.busy.is_none()
            && p.details
                .is_some_and(|d| d.online && d.can("file_transfer"));
        f.button(
            "Upload file…",
            Some(icons::UPLOAD),
            Action::Upload,
            can_transfer,
        );
        f.skip(6);
        f.button(
            "Download file…",
            Some(icons::DOWNLOAD),
            Action::Download,
            can_transfer,
        );
        f.skip(10);
        let zone = Rect::new(f.x, f.y, f.w, m.px(72));
        f.items.push(Item::DropZone { rect: zone });
        let hint = "or drop files here";
        f.items.push(Item::Text {
            x: zone.x + zone.w.saturating_sub(render::text_width(hint, m.label())) / 2,
            y: m.text_y(zone, m.label()),
            style: m.label(),
            color: t.muted,
            text: hint.into(),
        });
        f.y += zone.h;
        if let Some(busy) = p.busy {
            f.skip(8);
            f.text(m.label(), t.muted, busy);
        } else if p.details.is_some_and(|d| !d.can("file_transfer")) {
            f.skip(8);
            f.text(m.label(), t.muted, "No file access here.");
        }
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
        top >= panel.y && top < panel.bottom() && item.bottom() <= panel.bottom()
    }

    // --- overlays -----------------------------------------------------------

    /// A menu's box, and each item's row.
    pub fn menu_rects(&self, menu: &Menu) -> (Rect, Vec<Rect>) {
        let m = self.m();
        let widest = menu
            .items
            .iter()
            .map(|i| render::text_width(&i.label, m.body()))
            .max()
            .unwrap_or(0);
        // The check mark's column, then the label.
        let w = (2 * m.pad() + m.icon() + m.px(8) + widest).min(self.layout.window.w);
        let x = menu.anchor.x.min(self.layout.window.w.saturating_sub(w));
        let y = menu.anchor.bottom() + m.px(4);
        let row_h = m.px(28);
        let rows: Vec<Rect> = (0..menu.items.len() as u32)
            .map(|i| Rect::new(x, y + i * row_h, w, row_h))
            .collect();
        let h = rows.len() as u32 * row_h;
        (Rect::new(x, y, w, h), rows)
    }

    /// A dialog's box, its lines of text, and its buttons (the prompt also
    /// has a text field).
    fn dialog(&self, text: &str, buttons: [(&str, Action); 2], field: bool) -> DialogGeometry {
        let m = *self.m();
        let area = self.layout.desktop;
        let pad = m.panel_pad();
        let chars = PROMPT_CHARS
            .min(area.w.saturating_sub(2 * (m.gap() + pad)) / m.char_w())
            .max(8);
        let w = chars * m.char_w() + 2 * pad;
        let lines = wrap(text, chars as usize);
        let line_h = m.line_h(m.body());
        let text_h = lines.len() as u32 * line_h;
        let field_h = if field { m.field_h() + m.px(12) } else { 0 };
        let h = pad + text_h + m.px(6) + field_h + m.button_h() + pad;
        let x = area.x + area.w.saturating_sub(w) / 2;
        let y = area.y + area.h.saturating_sub(h) / 3;
        let bx = Rect::new(x, y, w, h);
        let field_y = y + pad + text_h + m.px(6);
        let field = field.then(|| Rect::new(x + pad, field_y, w - 2 * pad, m.field_h()));
        let buttons_y = y + h - pad - m.button_h();
        let mut right = bx.right() - pad;
        let mut rects = Vec::new();
        for (label, action) in buttons {
            let bw = render::text_width(label, m.body()) + 2 * m.pad();
            let rect = Rect::new(right.saturating_sub(bw), buttons_y, bw, m.button_h());
            right = rect.x.saturating_sub(m.gap());
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
        if self.layout.header.contains(pos) {
            return self
                .toolbar_buttons()
                .iter()
                .find_map(enabled)
                .map_or(Hit::Nothing, Hit::Action);
        }
        if self.layout.rail.contains(pos) {
            return self
                .rail_buttons()
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
            .chain(self.rail_buttons())
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

    fn text(&self, canvas: &mut Canvas, pos: (u32, u32), style: Style, color: u32, text: &str) {
        render::draw_text(canvas, self.glyphs, pos, style, color, text);
    }

    fn hovered(&self, rect: Rect) -> bool {
        self.cursor.is_some_and(|c| rect.contains(c))
    }

    /// A tab: its icon over its name; the open one marked down its left
    /// edge.
    fn draw_tab(&self, canvas: &mut Canvas, b: &Button) {
        let (m, t) = (self.m(), self.theme);
        let color = if b.active {
            render::fill_rect(canvas, b.rect, t.surface);
            // Inside the rail's own hairline.
            let mark = Rect::new(b.rect.x + m.border(), b.rect.y, m.px(2), b.rect.h);
            render::fill_rect(canvas, mark, t.heading);
            t.heading
        } else if self.hovered(b.rect) {
            t.bright
        } else {
            t.muted
        };
        let style = m.tab_label();
        let size = m.tab_icon();
        let top = b.rect.y + b.rect.h.saturating_sub(size + m.px(5) + style.px) / 2;
        if let Some(icon) = &b.icon {
            let x = b.rect.x + b.rect.w.saturating_sub(size) / 2;
            render::draw_icon(canvas, self.glyphs, (x, top), size, color, icon);
        }
        // Centred as drawn: nothing follows the last character.
        let w = render::text_width(&b.label, style).saturating_sub(style.tracking);
        let x = b.rect.x + b.rect.w.saturating_sub(w) / 2;
        self.text(canvas, (x, top + size + m.px(5)), style, color, &b.label);
    }

    fn draw_button(&self, canvas: &mut Canvas, b: &Button) {
        if b.kind == Kind::Tab {
            return self.draw_tab(canvas, b);
        }
        let (m, t) = (self.m(), self.theme);
        let hovered = b.enabled && self.hovered(b.rect);
        let (mut fill, mut edge, mut ink, mut icon_ink) = match b.kind {
            Kind::Danger => (
                t.danger_fill,
                if hovered {
                    t.danger_text
                } else {
                    t.danger_line
                },
                t.danger_text,
                t.danger_text,
            ),
            _ => (
                t.control,
                if hovered {
                    t.focus
                } else if b.active {
                    t.heading
                } else {
                    t.line
                },
                if hovered { t.bright } else { t.text },
                if b.active { t.heading } else { t.muted },
            ),
        };
        if !b.enabled {
            // Faded into the surface it is on, the label into the button.
            fill = t.disabled(fill, t.surface);
            edge = t.disabled(edge, t.surface);
            ink = t.disabled(ink, fill);
            icon_ink = t.disabled(icon_ink, fill);
        }
        render::fill_round_rect(canvas, b.rect, m.radius(), fill);
        render::outline_round_rect(canvas, b.rect, m.radius(), m.border(), edge);
        let body = m.body();
        let down = |height: u32| b.rect.y + b.rect.h.saturating_sub(height) / 2;
        if b.kind == Kind::Select {
            let caption_x = b
                .rect
                .x
                .saturating_sub(m.px(6) + render::text_width(&b.caption, body));
            self.text(
                canvas,
                (caption_x, down(body.px)),
                body,
                t.muted,
                &b.caption,
            );
            self.text(
                canvas,
                (b.rect.x + m.px(6), down(body.px)),
                body,
                ink,
                &b.label,
            );
            let size = m.px(8);
            let x = b.rect.right().saturating_sub(m.px(6) + size);
            render::draw_caret(canvas, (x, down(size / 2)), size, icon_ink);
            return;
        }
        let size = m.icon();
        let mut x = b.rect.x + m.pad();
        if let Some(icon) = &b.icon {
            if b.label.is_empty() {
                x = b.rect.x + b.rect.w.saturating_sub(size) / 2;
            }
            render::draw_icon(canvas, self.glyphs, (x, down(size)), size, icon_ink, icon);
            x += size + m.px(8);
        }
        self.text(canvas, (x, down(body.px)), body, ink, &b.label);
    }

    /// A text field holding `text`; with the keyboard (`focused`), edged
    /// and with the caret after the text. `hint`: an icon before the
    /// text, and what to say while there is none.
    fn draw_field(
        &self,
        canvas: &mut Canvas,
        field: Rect,
        text: &str,
        focused: bool,
        hint: Option<(&Icon, &str)>,
    ) {
        let (m, t) = (self.m(), self.theme);
        let body = m.body();
        render::fill_round_rect(canvas, field, m.radius(), t.window);
        let edge = if focused { t.focus } else { t.line };
        render::outline_round_rect(canvas, field, m.radius(), m.border(), edge);
        let mut tx = field.x + m.pad();
        let ty = m.text_y(field, body);
        if let Some((icon, _)) = hint {
            let y = field.y + field.h.saturating_sub(m.icon()) / 2;
            render::draw_icon(canvas, self.glyphs, (tx, y), m.icon(), t.muted, icon);
            tx += m.icon() + m.px(8);
        }
        let room = (field.right().saturating_sub(m.pad() + tx) / m.char_w()) as usize;
        if let (Some((_, placeholder)), true, false) = (hint, text.is_empty(), focused) {
            return self.text(
                canvas,
                (tx, ty),
                body,
                t.muted,
                &truncate(placeholder, room),
            );
        }
        // Show the end of a long entry: that is where typing goes.
        let room = room.saturating_sub(1);
        let count = text.chars().count();
        let shown: String = text.chars().skip(count.saturating_sub(room)).collect();
        self.text(canvas, (tx, ty), body, t.text, &shown);
        if focused {
            let caret_x = tx + render::text_width(&shown, body) + m.border();
            render::fill_rect(canvas, Rect::new(caret_x, ty, m.border(), body.px), t.text);
        }
    }

    fn draw_notices(&self, canvas: &mut Canvas) {
        let (m, t) = (*self.m(), self.theme);
        let area = self.layout.desktop;
        let (body, inset, gap) = (m.body(), m.pad(), m.px(8));
        let chars =
            NOTICE_CHARS.min((area.w.saturating_sub(2 * (gap + inset)) / m.char_w()) as usize);
        let line_h = m.line_h(body);
        let mut bottom = area.bottom().saturating_sub(gap);
        for notice in self.notices.iter().rev() {
            let lines = wrap(&notice.text, chars.max(8));
            let w = lines
                .iter()
                .map(|l| render::text_width(l, body))
                .max()
                .unwrap_or(0)
                + 2 * inset;
            let h = (lines.len() as u32 - 1) * line_h + body.px + 2 * inset;
            if bottom < area.y + h {
                break;
            }
            let rect = Rect::new(area.x + gap, bottom - h, w, h);
            let (fill, edge) = if notice.error {
                (t.danger_fill, t.danger_line)
            } else {
                (t.control, t.line)
            };
            render::fill_round_rect(canvas, rect, m.radius(), fill);
            render::outline_round_rect(canvas, rect, m.radius(), m.border(), edge);
            for (i, line) in lines.iter().enumerate() {
                let y = rect.y + inset + i as u32 * line_h;
                self.text(canvas, (rect.x + inset, y), body, t.text, line);
            }
            bottom = rect.y.saturating_sub(gap);
        }
    }

    fn draw_header(&self, canvas: &mut Canvas) {
        let (m, t) = (*self.m(), self.theme);
        let h = self.layout.header;
        render::fill_rect(canvas, h, t.surface);
        let line = Rect::new(h.x, h.bottom().saturating_sub(m.border()), h.w, m.border());
        render::fill_rect(canvas, line, t.line);
        let header = self.header();
        render::draw_mark(
            canvas,
            self.glyphs,
            (header.mark.x, header.mark.y),
            header.mark.w.min(h.h),
            t.mark,
        );
        let (host_x, host) = &header.host;
        let y = m.text_y(h, m.strong());
        self.text(canvas, (*host_x, y), m.strong(), t.bright, host);
        if header
            .buttons
            .first()
            .is_some_and(|b| b.kind == Kind::Select)
        {
            render::fill_rect(canvas, header.divider, t.line);
        }
        for b in &header.buttons {
            self.draw_button(canvas, b);
        }
    }

    fn draw_rail(&self, canvas: &mut Canvas) {
        let (m, t) = (self.m(), self.theme);
        let rail = self.layout.rail;
        render::fill_rect(canvas, rail, t.rail);
        for b in &self.rail_buttons() {
            self.draw_button(canvas, b);
        }
        render::fill_rect(
            canvas,
            Rect {
                w: m.border(),
                ..rail
            },
            t.line,
        );
    }

    fn draw_footer(&self, canvas: &mut Canvas) {
        let (m, t) = (self.m(), self.theme);
        let f = self.layout.footer;
        render::fill_rect(canvas, f, t.surface);
        render::fill_rect(
            canvas,
            Rect {
                h: m.border().min(f.h),
                ..f
            },
            t.line,
        );
        let style = m.label();
        for item in self.footer_items() {
            let mut x = item.x;
            if item.dot {
                let d = m.px(6);
                let dot = Rect::new(x, f.y + f.h.saturating_sub(d) / 2, d, d);
                render::fill_round_rect(canvas, dot, d / 2, item.color);
                x += m.px(12);
            }
            self.text(
                canvas,
                (x, m.text_y(f, style)),
                style,
                item.color,
                &item.text,
            );
        }
    }

    fn draw_item(&self, canvas: &mut Canvas, item: &Item) {
        let (m, t) = (self.m(), self.theme);
        match item {
            Item::Text {
                x,
                y,
                style,
                color,
                text,
            } => self.text(canvas, (*x, *y), *style, *color, text),
            Item::Bar { rect, fraction } => {
                let r = rect.h / 2;
                render::fill_round_rect(canvas, *rect, r, t.line);
                let fill = (f64::from(rect.w) * fraction.clamp(0.0, 1.0)).round() as u32;
                let color = match fraction {
                    f if *f >= 0.9 => t.danger,
                    f if *f >= 0.8 => t.warn,
                    _ => t.ok,
                };
                render::fill_round_rect(canvas, Rect { w: fill, ..*rect }, r, color);
            }
            Item::Button(b) => self.draw_button(canvas, b),
            Item::Field {
                rect,
                text,
                focused,
            } => {
                let hint = (&icons::SEARCH, "Type a name, press Enter");
                self.draw_field(canvas, *rect, text, *focused, Some(hint));
            }
            Item::Row {
                rect,
                name,
                detail,
                selected,
                ..
            } => {
                if *selected {
                    render::fill_round_rect(canvas, *rect, m.radius(), t.selected);
                } else if self.hovered(*rect) {
                    render::fill_round_rect(canvas, *rect, m.radius(), t.control);
                }
                let tx = rect.x + m.pad();
                let ty = rect.y + m.px(6);
                self.text(canvas, (tx, ty), m.body(), t.text, name);
                let ty = ty + m.body().px + m.px(5);
                self.text(canvas, (tx, ty), m.label(), t.muted, detail);
            }
            Item::Rule { rect } => render::fill_rect(canvas, *rect, t.line),
            Item::Card { rect } => {
                render::fill_round_rect(canvas, *rect, m.radius(), t.window);
                render::outline_round_rect(canvas, *rect, m.radius(), m.border(), t.line);
            }
            Item::DropZone { rect } => {
                render::dashed_outline(canvas, *rect, m.border(), m.px(4), t.line)
            }
        }
    }

    fn draw_panel(&self, canvas: &mut Canvas, panel: Rect) {
        let (m, t) = (self.m(), self.theme);
        render::fill_rect(canvas, panel, t.surface);
        render::fill_rect(
            canvas,
            Rect {
                w: m.border(),
                ..panel
            },
            t.line,
        );
        let (items, height) = self.panel_items();
        for item in items.iter().filter(|i| self.visible(i)) {
            self.draw_item(canvas, item);
        }
        // A scroll bar when it does not all fit.
        if height > panel.h {
            let track = panel.h;
            let thumb = (u64::from(track) * u64::from(panel.h) / u64::from(height)) as u32;
            let top = (u64::from(track - thumb) * u64::from(self.scroll)
                / u64::from((height - panel.h).max(1))) as u32;
            let w = m.px(3);
            render::fill_round_rect(
                canvas,
                Rect::new(panel.right() - w, panel.y + top, w, thumb.max(m.gap())),
                w / 2,
                t.line,
            );
        }
    }

    fn draw_overlay(&self, canvas: &mut Canvas, overlay: &Overlay) {
        let (m, t) = (*self.m(), self.theme);
        let body = m.body();
        if let Overlay::Menu(menu) = overlay {
            let (bx, rows) = self.menu_rects(menu);
            render::fill_round_rect(canvas, bx, m.radius(), t.surface);
            for (item, row) in menu.items.iter().zip(&rows) {
                if self.hovered(*row) {
                    render::fill_round_rect(canvas, *row, m.radius(), t.control);
                }
                let color = if item.checked { t.heading } else { t.text };
                let x = row.x + m.pad();
                if item.checked {
                    let y = row.y + row.h.saturating_sub(m.icon()) / 2;
                    render::draw_icon(canvas, self.glyphs, (x, y), m.icon(), color, &icons::CHECK);
                }
                let x = x + m.icon() + m.px(8);
                self.text(canvas, (x, m.text_y(*row, body)), body, color, &item.label);
            }
            return render::outline_round_rect(canvas, bx, m.radius(), m.border(), t.line);
        }
        let g = self.overlay_geometry(overlay).expect("a dialog");
        render::fill_round_rect(canvas, g.rect, m.radius(), t.surface);
        render::outline_round_rect(canvas, g.rect, m.radius(), m.border(), t.focus);
        let pad = m.panel_pad();
        for (i, line) in g.lines.iter().enumerate() {
            let y = g.rect.y + pad + i as u32 * m.line_h(body);
            self.text(canvas, (g.rect.x + pad, y), body, t.text, line);
        }
        if let (Some(field), Overlay::Prompt(prompt)) = (g.field, overlay) {
            if prompt.masked {
                let stars = "*".repeat(prompt.text.chars().count());
                self.draw_field(canvas, field, &stars, true, None);
            } else {
                self.draw_field(canvas, field, &prompt.text, true, None);
            }
        }
        for b in &g.buttons {
            self.draw_button(canvas, b);
        }
    }

    /// Draw the controls over a canvas that already holds the picture.
    pub fn draw(&self, canvas: &mut Canvas) {
        self.draw_header(canvas);
        self.draw_rail(canvas);
        if let Some(panel) = self.layout.panel {
            self.draw_panel(canvas, panel);
        }
        self.draw_footer(canvas);
        self.draw_notices(canvas);
        // Menus and dialogs, on top.
        if let Some(overlay) = self.overlay {
            self.draw_overlay(canvas, overlay);
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

    /// Text 16 pixels high: a third larger than the design.
    const M: Metrics = Metrics { font: 16 };

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
        theme: Theme,
        glyphs: Glyphs,
        monitors: Vec<MonitorInfo>,
        details: AgentDetails,
        commands: Vec<QuickCommand>,
        overlay: Option<Overlay>,
        vault: Vault,
        tab: Option<Tab>,
    }

    impl Fixture {
        fn new() -> Self {
            Self::on(Tab::Vault)
        }

        fn on(tab: Tab) -> Self {
            Self {
                theme: Theme::default(),
                glyphs: Glyphs::new(),
                monitors: vec![monitor(0, true), monitor(1, false)],
                details: details(),
                commands: default_commands(),
                overlay: None,
                vault: Vault::default(),
                tab: Some(tab),
            }
        }

        fn chrome(&self, width: u32, height: u32) -> Chrome<'_> {
            Chrome {
                metrics: M,
                layout: layout(width, height, &M, self.tab.is_some()),
                theme: &self.theme,
                glyphs: &self.glyphs,
                tab: self.tab,
                toolbar: Toolbar {
                    host: "WS-01",
                    display: DisplayMode::Fill,
                    frame_rate: FrameRate::Auto,
                    streamed_fps: Some(60),
                    monitors: &self.monitors,
                    active_monitor: Some(1),
                    font_px: 12,
                    fullscreen: false,
                },
                footer: Footer {
                    link: "encrypted · direct",
                    live: true,
                    fps: 30,
                },
                panel: Panel {
                    api: true,
                    details: Some(&self.details),
                    error: None,
                    commands: &self.commands,
                    busy: None,
                    lent: None,
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
                Item::Rule { .. } => Some("---".into()),
                Item::DropZone { .. } => Some("(drop)".into()),
                Item::Bar { .. } | Item::Card { .. } => None,
            })
            .collect()
    }

    fn draw(chrome: &Chrome) -> Vec<u32> {
        let (w, h) = (chrome.layout.window.w, chrome.layout.window.h);
        let mut pixels = vec![0; (w * h) as usize];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            width: w,
            height: h,
        };
        chrome.draw(&mut canvas);
        pixels
    }

    #[test]
    fn layout_puts_the_rail_right_the_panel_beside_it_and_the_picture_in_between() {
        let l = layout(1600, 900, &M, true);
        let (top, bottom) = (M.header_h(), 900 - M.footer_h());
        assert_eq!(l.header, Rect::new(0, 0, 1600, top));
        assert_eq!(l.footer, Rect::new(0, bottom, 1600, M.footer_h()));
        assert_eq!(
            l.rail,
            Rect::new(1600 - M.tab(), top, M.tab(), bottom - top)
        );
        let panel = l.panel.unwrap();
        assert_eq!(
            (panel.right(), panel.y, panel.bottom(), panel.w),
            (l.rail.x, top, bottom, M.panel_w())
        );
        assert_eq!(l.desktop, Rect::new(0, top, panel.x, bottom - top));
        // No tab open, or no room for its panel: the picture takes the
        // width, up to the rail, which is always there.
        assert_eq!(layout(1600, 900, &M, false).desktop.w, l.rail.x);
        let narrow = layout(700, 900, &M, true);
        assert_eq!((narrow.panel, narrow.desktop.w), (None, narrow.rail.x));
    }

    #[test]
    fn metrics_follow_the_text_size_and_the_scale_factor() {
        // The default size is the design's: its lengths come out as drawn.
        let m = Metrics::for_scale_factor(1.0);
        assert_eq!((m.font, DEFAULT_FONT_PX), (12, 9));
        assert_eq!(
            [
                m.header_h(),
                m.button_h(),
                m.tab(),
                m.panel_w(),
                m.footer_h()
            ],
            [48, 32, 56, 320, 28]
        );
        assert_eq!((m.label().px, m.value().px, m.char_w()), (11, 13, 7));
        // Larger text, or a denser screen, and everything grows with it.
        assert_eq!(Metrics::new(12, 1.0), M);
        assert_eq!(Metrics::new(9, 2.0).font, 24);
        assert_eq!(Metrics::new(9, 1.5).panel_w(), 480);
        assert_eq!(Metrics::new(9, 0.5).font, 12);
        // Text sits centred in a button.
        assert_eq!(m.text_y(Rect::new(0, 100, 50, m.button_h()), m.body()), 110);
        // Out-of-range sizes are clamped.
        assert_eq!(Metrics::new(1, 1.0), Metrics::new(MIN_FONT_PX, 1.0));
        assert_eq!(Metrics::new(500, 1.0), Metrics::new(MAX_FONT_PX, 1.0));
    }

    #[test]
    fn the_text_size_display_mode_and_tab_are_remembered_together() {
        let dir = std::env::temp_dir().join(format!("rmm-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("viewer.json");
        assert_eq!(load_display(&path), None);
        assert_eq!(load_panel(&path), None);
        save_setting(&path, "font_size", 9.into()).unwrap();
        assert_eq!(load_display(&path), None);
        save_setting(&path, "display", "fill".into()).unwrap();
        save_setting(&path, "font_size", 11.into()).unwrap();
        save_setting(&path, "panel", Tab::Tools.key().into()).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({ "font_size": 11, "display": "fill", "panel": "tools" })
        );
        assert_eq!(load_display(&path), Some(DisplayMode::Fill));
        assert_eq!(load_panel(&path), Some(Some(Tab::Tools)));
        assert!(!path.with_extension("tmp").exists());
        // Every mode and tab survives the round trip, as the viewer saves it.
        for mode in DisplayMode::ALL {
            save_setting(&path, "display", mode.label().to_lowercase().into()).unwrap();
            assert_eq!(load_display(&path), Some(mode));
        }
        for tab in Tab::ALL {
            save_setting(&path, "panel", tab.key().into()).unwrap();
            assert_eq!(load_panel(&path), Some(Some(tab)));
        }
        // A closed panel is remembered too; a tab this viewer does not
        // have is not.
        save_setting(&path, "panel", PANEL_CLOSED.into()).unwrap();
        assert_eq!(load_panel(&path), Some(None));
        save_setting(&path, "panel", "chat".into()).unwrap();
        assert_eq!(load_panel(&path), None);
        // Nonsense in the file is no mode, and is replaced by the next save.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_display(&path), None);
        save_setting(&path, "display", "scale".into()).unwrap();
        assert_eq!(load_display(&path), Some(DisplayMode::Scale));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_menu_offers_sizes_and_marks_the_current_one() {
        let menu = text_menu(Rect::default(), 9);
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "8 px",
                "9 px (default)",
                "10 px",
                "11 px",
                "12 px",
                "14 px",
                "16 px",
                "20 px"
            ]
        );
        assert!(menu.items[1].checked && menu.items[1].action == Action::SetTextSize(9));
        // A size set on the command line shows up too.
        let menu = text_menu(Rect::default(), 13);
        assert!(menu.items.iter().any(|i| i.checked && i.label == "13 px"));
    }

    #[test]
    fn the_header_holds_the_four_menus_and_the_sessions_buttons() {
        let f = Fixture::new();
        let chrome = f.chrome(2000, 900);
        let buttons = chrome.toolbar_buttons();
        let labels: Vec<(&str, &str)> = buttons
            .iter()
            .map(|b| (b.caption.as_str(), b.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                ("Display", "Fill"),
                ("FPS", "Auto (60)"),
                ("Monitor", "2 of 2"),
                ("Text", "12"),
                ("", ""),
                ("", ""),
                ("", "Ctrl+Alt+Del"),
                ("", "Disconnect"),
            ]
        );
        assert!(buttons[..4].iter().all(|b| b.kind == Kind::Select));
        // Refresh and full screen are only their icons; Disconnect stands
        // out, at the right edge.
        assert_eq!(
            buttons[4..]
                .iter()
                .map(|b| b.icon.map(|i| i.name))
                .collect::<Vec<_>>(),
            [Some("refresh"), Some("fullscreen"), None, Some("power")]
        );
        assert_eq!(buttons[7].kind, Kind::Danger);
        assert_eq!(buttons[7].rect.right(), 2000 - M.px(16));
        assert_eq!(buttons[4].rect.w, buttons[4].rect.h);
        // Left to right, none over another, all inside the header.
        for pair in buttons.windows(2) {
            assert!(pair[0].rect.right() < pair[1].rect.x, "{pair:?}");
        }
        assert!(buttons
            .iter()
            .all(|b| b.rect.y > 0 && b.rect.bottom() < chrome.layout.header.bottom()));
        let actions = [
            Action::DisplayMenu,
            Action::FrameRateMenu,
            Action::MonitorMenu,
            Action::TextMenu,
            Action::Keyframe,
            Action::ToggleFullscreen,
            Action::SecureAttention,
            Action::Disconnect,
        ];
        for (button, action) in buttons.iter().zip(actions) {
            assert_eq!(chrome.hit(centre(button.rect)), Hit::Action(action));
        }
        assert_eq!(chrome.hit((800.0, 500.0)), Hit::Desktop);
        // The machine's name, between the mark and the menus.
        assert_eq!(chrome.hit((M.px(60) as f64, 30.0)), Hit::Nothing);
        // Full screen shows when it is on; the monitor is unknown until
        // the agent says.
        let mut chrome = f.chrome(2000, 900);
        chrome.toolbar.fullscreen = true;
        chrome.toolbar.active_monitor = None;
        let buttons = chrome.toolbar_buttons();
        assert!(buttons[5].active && !buttons[4].active);
        assert_eq!(buttons[2].label, "-");
    }

    #[test]
    fn a_narrow_header_leaves_out_menus_rather_than_overlapping() {
        let f = Fixture::new();
        let selects = |width| -> Vec<String> {
            f.chrome(width, 600)
                .toolbar_buttons()
                .into_iter()
                .filter(|b| b.kind == Kind::Select)
                .map(|b| b.caption)
                .collect()
        };
        assert_eq!(selects(1600), ["Display", "FPS", "Monitor", "Text"]);
        let narrow = selects(1050);
        assert!(narrow.len() < 4, "{narrow:?}");
        assert_eq!(narrow[..2], ["Display", "FPS"], "display and FPS stay");
        for width in [300, 500, 700, 900, 1050, 1200] {
            let buttons = f.chrome(width, 600).toolbar_buttons();
            // The session's buttons are always all there.
            assert!(buttons.iter().any(|b| b.action == Action::Disconnect));
            let (left, right): (Vec<_>, Vec<_>) =
                buttons.iter().partition(|b| b.kind == Kind::Select);
            assert_eq!(right.len(), 4);
            let left_end = left.iter().map(|b| b.rect.right()).max().unwrap();
            let right_start = right.iter().map(|b| b.rect.x).min().unwrap();
            assert!(
                left.len() == 2 || left_end <= right_start,
                "{width}: {buttons:?}"
            );
        }
    }

    #[test]
    fn the_rail_opens_one_tab_at_a_time() {
        let mut f = Fixture::on(Tab::Status);
        let chrome = f.chrome(1600, 900);
        let tabs = chrome.rail_buttons();
        assert_eq!(
            tabs.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(),
            ["VAULT", "STATUS", "TOOLS"]
        );
        assert_eq!(
            tabs.iter().map(|b| b.active).collect::<Vec<_>>(),
            [false, true, false]
        );
        for (button, tab) in tabs.iter().zip(Tab::ALL) {
            assert!(button.kind == Kind::Tab && button.icon.is_some());
            assert_eq!(button.rect.w, M.tab());
            assert_eq!(
                chrome.hit(centre(button.rect)),
                Hit::Action(Action::Tab(tab))
            );
        }
        // Below the tabs the rail does nothing.
        assert_eq!(chrome.hit((1590.0, 700.0)), Hit::Nothing);
        // Only the open tab's contents are in the panel.
        let (items, _) = chrome.panel_items();
        assert_eq!(texts(&items)[0], "STATUS");
        assert!(!texts(&items).contains(&"[Unlock vault]".to_owned()));
        // With none open there is no panel, but the tabs are still there.
        f.tab = None;
        let chrome = f.chrome(1600, 900);
        assert_eq!(chrome.layout.panel, None);
        assert_eq!(chrome.panel_items(), (Vec::new(), 0));
        assert!(chrome.rail_buttons().iter().all(|b| !b.active));
        assert_eq!(
            chrome.hit(centre(chrome.rail_buttons()[2].rect)),
            Hit::Action(Action::Tab(Tab::Tools))
        );
        // A window too short for all three shows the ones that fit.
        assert_eq!(f.chrome(1600, 270).rail_buttons().len(), 2);
    }

    #[test]
    fn the_status_tab_shows_what_the_agent_says() {
        let f = Fixture::on(Tab::Status);
        let chrome = f.chrome(1600, 1200);
        let (items, _) = chrome.panel_items();
        assert_eq!(
            texts(&items),
            [
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
                "Windows 11 Pro (build 26100)",
                "Disk C:\\",
                "45%",
                "112.0 GiB of 250.0 GiB used",
            ]
        );
        // The two addresses sit side by side.
        let at = |wanted: &str| {
            items
                .iter()
                .find_map(|i| match i {
                    Item::Text { x, y, text, .. } if text == wanted => Some((*x, *y)),
                    _ => None,
                })
                .unwrap()
        };
        let (internal, external) = (at("Internal IP"), at("External IP"));
        assert!(internal.1 == external.1 && internal.0 < external.0);
        // The disk is a card with its bar, 45% full, inside it.
        let card = items.iter().find_map(|i| match i {
            Item::Card { rect } => Some(*rect),
            _ => None,
        });
        let bar = items.iter().find_map(|i| match i {
            Item::Bar { rect, fraction } => Some((*rect, *fraction)),
            _ => None,
        });
        let (card, (bar, fraction)) = (card.unwrap(), bar.unwrap());
        assert!((fraction - 0.448).abs() < 0.001);
        assert!(card.contains(centre(bar)) && bar.right() < card.right());
        let panel = chrome.layout.panel.unwrap();
        assert!(card.x > panel.x && card.right() < panel.right());

        // An address too long for its column wraps inside it.
        let mut f = Fixture::on(Tab::Status);
        f.details.local_ip = Some("2001:db8:85a3:8d3:1319:8a2e:370:7348".into());
        f.details.online = false;
        let mut chrome = f.chrome(1600, 1200);
        chrome.panel.error = Some("timed out");
        let (items, _) = chrome.panel_items();
        let all = texts(&items);
        // (In its own column: the other address is still beside it.)
        assert!(all.contains(&"2001:db8:85a3:8d3".to_owned()), "{all:?}");
        assert!(all.contains(&"203.0.113.9".to_owned()));
        assert!(all.contains(&"The agent is offline.".to_owned()));
        assert!(all.contains(&"Not updating: timed out".to_owned()));
        // Nothing heard yet.
        chrome.panel.details = None;
        chrome.panel.error = None;
        assert_eq!(texts(&chrome.panel_items().0), ["STATUS", "Loading..."]);
    }

    #[test]
    fn the_tools_tab_has_the_commands_and_file_transfer() {
        let mut f = Fixture::on(Tab::Tools);
        f.commands.push("Services=services.msc".parse().unwrap());
        let chrome = f.chrome(1600, 1200);
        let (items, _) = chrome.panel_items();
        assert_eq!(
            texts(&items),
            [
                "COMMANDS + FILES",
                "Commands",
                "[Command prompt]",
                "[Network connections]",
                "[Remote desktop]",
                "[Services]",
                "Files",
                "[Upload file…]",
                "[Download file…]",
                "(drop)",
                "or drop files here",
            ]
        );
        // Each command has the icon of what it starts; every button works.
        let icons: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                Item::Button(b) => b.icon.map(|i| i.name),
                _ => None,
            })
            .collect();
        assert_eq!(
            icons,
            ["terminal", "network", "screen", "gear", "upload", "download"]
        );
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
        // The drop zone is only a hint: a click on it does nothing.
        let zone = items.iter().find_map(|i| match i {
            Item::DropZone { rect } => Some(*rect),
            _ => None,
        });
        assert_eq!(chrome.hit(centre(zone.unwrap())), Hit::Nothing);
        // No commands: say where they come from.
        f.commands.clear();
        let all = texts(&f.chrome(1600, 1200).panel_items().0);
        assert!(all.contains(&"None: add some in the TUI.".to_owned()));
    }

    #[test]
    fn buttons_are_disabled_without_access_or_while_busy() {
        let mut f = Fixture::on(Tab::Tools);
        f.details.capabilities = vec!["desktop".into()];
        let chrome = f.chrome(1600, 1200);
        let (items, _) = chrome.panel_items();
        assert!(button(&items, "Command prompt").enabled);
        let upload = button(&items, "Upload file…");
        assert!(!upload.enabled);
        assert_eq!(chrome.hit(centre(upload.rect)), Hit::Nothing);
        assert!(texts(&items).contains(&"No file access here.".to_owned()));

        // Offline: nothing to launch or transfer.
        f.details.online = false;
        f.details.capabilities.push("file_transfer".into());
        let chrome = f.chrome(1600, 1200);
        let (items, _) = chrome.panel_items();
        assert!(items.iter().all(|i| !matches!(i, Item::Button(b)
            if b.enabled && matches!(b.action, Action::Launch(_) | Action::Upload | Action::Download))));

        // Busy: no second transfer.
        let f = Fixture::on(Tab::Tools);
        let mut chrome = f.chrome(1600, 1200);
        chrome.panel.busy = Some("Uploading a.zip: 40%");
        let (items, _) = chrome.panel_items();
        assert!(texts(&items).contains(&"Uploading a.zip: 40%".to_owned()));
        assert!(items
            .iter()
            .any(|i| matches!(i, Item::Button(b) if b.action == Action::Upload && !b.enabled)));
    }

    #[test]
    fn without_the_api_the_panel_says_how_to_get_it() {
        let enabled = |tab: Tab| -> (String, Vec<String>) {
            let f = Fixture::on(tab);
            let mut chrome = f.chrome(1600, 1200);
            chrome.panel.api = false;
            chrome.panel.details = None;
            let (items, _) = chrome.panel_items();
            let buttons = items
                .iter()
                .filter_map(|i| match i {
                    Item::Button(b) if b.enabled => Some(b.label.clone()),
                    _ => None,
                })
                .collect();
            (texts(&items).join(" "), buttons)
        };
        let (status, _) = enabled(Tab::Status);
        assert!(status.contains("Start the viewer from the TUI"), "{status}");
        // The vault and the lent password need no API; the tools do.
        assert_eq!(enabled(Tab::Tools).1, Vec::<String>::new());
        assert_eq!(
            enabled(Tab::Vault).1,
            ["Unlock vault", "Ask user", "Type", "Forget"]
        );
        // Nor do the session's own buttons, or the footer's first words.
        let f = Fixture::new();
        let mut chrome = f.chrome(1600, 1200);
        chrome.panel.details = None;
        assert!(chrome.toolbar_buttons().iter().all(|b| b.enabled));
        let footer: Vec<String> = chrome.footer_items().into_iter().map(|i| i.text).collect();
        assert_eq!(footer, ["encrypted · direct", "30 fps"]);
    }

    #[test]
    fn the_footer_says_how_the_session_is_going() {
        let f = Fixture::new();
        let chrome = f.chrome(1600, 900);
        let items = chrome.footer_items();
        let said: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            said,
            [
                "encrypted · direct",
                "30 fps",
                "CORP\\alice",
                "192.168.1.20",
                "Win 11 Pro",
                "C:\\ 45%"
            ]
        );
        // The session's state is green behind its dot; the disk is at the
        // right edge; nothing runs into what follows it.
        assert!(items[0].dot && items[0].color == f.theme.ok_text);
        assert!(items[1..]
            .iter()
            .all(|i| !i.dot && i.color == f.theme.muted));
        let style = M.label();
        let disk = &items[5];
        assert_eq!(
            disk.x + render::text_width(&disk.text, style),
            1600 - M.px(16)
        );
        for pair in items.windows(2) {
            assert!(
                pair[0].x + render::text_width(&pair[0].text, style) < pair[1].x,
                "{pair:?}"
            );
        }
        // A narrow window drops from the end of the left group.
        let narrow: Vec<String> = f
            .chrome(520, 900)
            .footer_items()
            .into_iter()
            .map(|i| i.text)
            .collect();
        assert_eq!(narrow[0], "encrypted · direct");
        assert!(narrow.len() < 6 && narrow.ends_with(&["C:\\ 45%".to_owned()]));
        // No session: why, plainly, and no frame rate.
        let mut chrome = f.chrome(1600, 900);
        chrome.footer = Footer {
            link: "Connecting...",
            live: false,
            fps: 0,
        };
        let items = chrome.footer_items();
        assert_eq!(items[0].text, "Connecting...");
        assert!(!items[0].dot && items[0].color == f.theme.muted);
        assert!(items.iter().all(|i| !i.text.ends_with("fps")));
        // The footer itself does nothing when clicked.
        assert_eq!(chrome.hit((100.0, 890.0)), Hit::Nothing);
        assert_eq!(
            short_os("Windows 11 IoT Enterprise LTSC 2024 (build 26100)"),
            "Win 11 IoT LTSC 2024"
        );
        assert_eq!(short_os("Ubuntu 24.04"), "Ubuntu 24.04");
    }

    #[test]
    fn a_short_window_scrolls_the_panel() {
        let f = Fixture::on(Tab::Tools);
        let chrome = f.chrome(1600, 400);
        let max = chrome.max_scroll();
        assert!(max > 0);
        // Scrolled to the bottom, the last button is on screen and works...
        let scrolled = Chrome {
            scroll: max,
            ..f.chrome(1600, 400)
        };
        let (items, _) = scrolled.panel_items();
        let download = button(&items, "Download file…").rect;
        assert!(download.bottom() <= scrolled.layout.footer.y);
        assert_eq!(
            scrolled.hit(centre(download)),
            Hit::Action(Action::Download)
        );
        // ...and not before scrolling: it is below the panel.
        let (items, _) = chrome.panel_items();
        let download = button(&items, "Download file…").rect;
        assert!(download.y >= chrome.layout.footer.y);
        assert_eq!(chrome.hit(centre(download)), Hit::Nothing);
        // A tall window has nothing to scroll.
        assert_eq!(f.chrome(1600, 1200).max_scroll(), 0);
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
        assert_eq!((bx.x, bx.y), (anchor.x, anchor.bottom() + M.px(4)));
        assert_eq!(
            chrome.hit(centre(rows[1])),
            Hit::Action(Action::SetDisplay(DisplayMode::Stretch))
        );
        assert_eq!(chrome.hit((800.0, 800.0)), Hit::Dismiss);
        // Under the right edge, a menu stays inside the window.
        let edge = Menu {
            anchor: Rect::new(1590, 10, 5, 20),
            items: menu.items.clone(),
        };
        assert_eq!(chrome.menu_rects(&edge).0.right(), 1600);

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
        assert_eq!(frame_rate_label(FrameRate::Auto, None), "Auto");
        assert_eq!(frame_rate_label(FrameRate::Auto, Some(60)), "Auto (60)");
        assert_eq!(frame_rate_label(FrameRate::Max, Some(240)), "Max");
        assert_eq!(frame_rate_label(FrameRate::Fixed(15), Some(15)), "15");
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
        let field = g.field.unwrap();
        let labels: Vec<&str> = g.buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["OK", "Cancel"]);
        // The text, the field and the buttons, top down, inside the box.
        assert!(g.rect.y < field.y && field.bottom() < g.buttons[0].rect.y);
        assert!(g.buttons.iter().all(|b| b.rect.bottom() < g.rect.bottom()
            && b.rect.x > g.rect.x
            && b.rect.right() < g.rect.right()));
        assert_eq!(
            chrome.hit(centre(g.buttons[0].rect)),
            Hit::Action(Action::PromptOk)
        );
        // Nothing else reacts while it is open.
        assert_eq!(chrome.hit((5.0, 5.0)), Hit::Nothing);
        assert_eq!(
            chrome.hit(centre(chrome.rail_buttons()[1].rect)),
            Hit::Nothing
        );

        // Everything draws, in the theme's colours.
        let notices = [
            Notice {
                text: "Uploaded a.zip (1.0 MiB) to C:\\Users\\Public\\Desktop\\a.zip".into(),
                error: false,
                until: Instant::now(),
            },
            Notice {
                text: "The user did not give a password.".into(),
                error: true,
                until: Instant::now(),
            },
        ];
        let pixels = draw(&Chrome {
            notices: &notices,
            cursor: Some((10.0, 10.0)),
            ..f.chrome(1600, 900)
        });
        let t = &f.theme;
        for color in [t.surface, t.control, t.line, t.danger_fill, t.focus, t.rail] {
            assert!(pixels.contains(&color), "{color:06X}");
        }
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

    /// The vault tab's items from under its title up to the rule before
    /// the lent password.
    fn vault_section(chrome: &Chrome) -> Vec<Item> {
        let (items, _) = chrome.panel_items();
        assert!(matches!(&items[0], Item::Text { text, .. } if text == "VAULT"));
        items
            .into_iter()
            .skip(1)
            .take_while(|i| !matches!(i, Item::Rule { .. }))
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
        let chrome = f.chrome(1600, 1200);
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
            texts(&vault_section(&f.chrome(1600, 1200))),
            ["[Unlock vault]", "Wrong master password."]
        );
        f.vault.busy = Some("Unlocking...".into());
        let section = vault_section(&f.chrome(1600, 1200));
        assert!(!button(&section, "Unlock vault").enabled);
        assert_eq!(texts(&section), ["[Unlock vault]", "Unlocking..."]);
    }

    #[test]
    fn the_lent_password_is_under_the_vault_and_says_what_it_has_heard() {
        let f = Fixture::new();
        let lent = |lent: Option<bool>| -> Vec<String> {
            let mut chrome = f.chrome(1600, 1200);
            chrome.panel.lent = lent;
            let (items, _) = chrome.panel_items();
            // Whatever it has heard, every choice is there: another
            // technician's viewer may have changed things since.
            for label in ["Ask user", "Type", "Forget"] {
                let b = button(&items, label);
                assert!(b.enabled, "{label}");
                assert_eq!(chrome.hit(centre(b.rect)), Hit::Action(b.action));
            }
            let all = texts(&items);
            let from = all.iter().position(|t| t == "---").unwrap();
            all[from + 1..].to_vec()
        };
        let unknown = lent(None);
        assert_eq!(unknown[0], "LENT PASSWORD");
        assert!(unknown[1].starts_with("Ask the user to type a password"));
        assert_eq!(
            unknown[unknown.len() - 3..],
            ["[Ask user]", "[Type]", "[Forget]"]
        );
        assert_eq!(lent(Some(true))[1], "stored");
        assert_eq!(lent(Some(false))[1], "none");
    }

    #[test]
    fn an_unlocked_vault_searches_lists_and_offers_what_each_item_has() {
        let mut f = Fixture::new();
        f.vault.unlocked = true;
        f.vault.query = "contoso".into();
        let chrome = f.chrome(1600, 1200);
        let section = vault_section(&chrome);
        assert_eq!(
            texts(&section),
            [
                "<contoso>",
                "[Type username]",
                "[Type password]",
                "[Type TOTP]",
                "[Sync]",
                "[Lock]",
            ]
        );
        // Nothing found yet: nothing to type. The field takes the keyboard.
        let kinds = ["Type username", "Type password", "Type TOTP"];
        assert!(kinds.iter().all(|l| !button(&section, l).enabled));
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
        let chrome = f.chrome(1600, 1200);
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
        // Item 1 has a username but no password or code; the three are
        // stacked, each as wide as the panel's contents, then Sync and
        // Lock side by side.
        let kinds = kinds.map(|l| button(&section, l));
        assert_eq!(kinds.clone().map(|b| b.enabled), [true, false, false]);
        assert!(kinds
            .iter()
            .all(|b| b.rect.x == field.x && b.rect.w == field.w));
        assert!(kinds[0].rect.bottom() < kinds[1].rect.y);
        let (sync, lock) = (button(&section, "Sync"), button(&section, "Lock"));
        assert!(sync.rect.y == lock.rect.y && sync.rect.right() < lock.rect.x);
        assert!(field.right() - lock.rect.right() <= 1);
        assert_eq!(
            chrome.hit(centre(kinds[0].rect)),
            Hit::Action(Action::VaultType(TextKind::Username))
        );
        assert_eq!(chrome.hit(centre(kinds[1].rect)), Hit::Nothing);

        // The list follows the selection.
        f.vault.select(10);
        let section = vault_section(&f.chrome(1600, 1200));
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
        let chrome = f.chrome(1600, 1200);
        let section = vault_section(&chrome);
        assert!(section
            .iter()
            .all(|i| !matches!(i, Item::Button(b) if b.enabled)));

        // The picked item and the field with the keyboard stand out.
        let pixels = draw(&chrome);
        assert!(pixels.contains(&f.theme.selected) && pixels.contains(&f.theme.window));
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
        let prompt = |text: &str, masked| {
            Some(Overlay::Prompt(Prompt {
                title: "Master password".into(),
                text: text.into(),
                purpose: PromptPurpose::VaultUnlock,
                masked,
            }))
        };
        f.overlay = prompt("hunter2", true);
        let masked = draw(&f.chrome(1600, 900));
        f.overlay = prompt("*******", false);
        assert!(masked == draw(&f.chrome(1600, 900)));
        f.overlay = prompt("hunter2", false);
        assert!(masked != draw(&f.chrome(1600, 900)));
    }

    #[test]
    fn the_search_field_says_what_to_do_until_it_has_the_keyboard() {
        let mut f = Fixture::new();
        f.vault.unlocked = true;
        let hint = draw(&f.chrome(1600, 900));
        f.vault.focused = true;
        let focused = draw(&f.chrome(1600, 900));
        assert!(hint != focused);
        // Focused, its edge is the theme's focus colour.
        assert!(focused.contains(&f.theme.focus) && !hint.contains(&f.theme.focus));
    }

    #[test]
    fn every_theme_and_tab_draws_at_any_size() {
        let light = crate::theme::Colors {
            foreground: brand::Rgb::hex(0x1F2328),
            background: brand::Rgb::hex(0xFFFFFF),
            surface: brand::Rgb::hex(0xF6F8FA),
            panel: brand::Rgb::hex(0xEAEEF2),
            dark: false,
            ..crate::theme::Colors::TETANUS
        };
        for theme in [Theme::default(), Theme::new(light)] {
            for tab in Tab::ALL.map(Some).into_iter().chain([None]) {
                let mut f = Fixture::new();
                f.theme = theme;
                f.tab = tab;
                f.vault.unlocked = true;
                f.vault.set_rows(vault_rows(3));
                for (w, h) in [(1, 1), (40, 30), (300, 60), (900, 200), (1600, 900)] {
                    let pixels = draw(&f.chrome(w, h));
                    // The header is the theme's surface, top-left (when
                    // it is more than its own bottom edge).
                    assert!(h == 1 || pixels[0] == theme.surface, "{w}x{h}");
                }
            }
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
