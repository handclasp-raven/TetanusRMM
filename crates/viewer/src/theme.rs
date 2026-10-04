//! The colours the viewer's controls are drawn in.
//!
//! They follow the TUI's theme: the TUI hands each viewer it starts the
//! colours of the theme picked there ([`THEME_ENV`]), and everything here
//! is worked out from those, so that a theme made in the TUI's editor
//! works as well as a bundled one. Started by hand, the viewer wears the
//! TUI's default theme.
//!
//! The roles are the design's (`branding/viewer`): a window, surfaces on
//! it (the header, the panel, the footer), controls on those, hairlines,
//! and a few colours that carry meaning.

use brand::theme::palette;
use brand::Rgb;

/// The environment variable the TUI's theme arrives in, as JSON:
/// `{"primary": "#FF9000", ..., "dark": true}`.
pub const THEME_ENV: &str = "RMM_VIEWER_THEME";

/// A theme of the TUI's: the colours of Textual's the viewer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub primary: Rgb,
    pub accent: Rgb,
    pub foreground: Rgb,
    pub background: Rgb,
    pub surface: Rgb,
    pub panel: Rgb,
    pub success: Rgb,
    pub warning: Rgb,
    pub error: Rgb,
    pub dark: bool,
}

impl Colors {
    /// The TUI's default theme (`tetanus` in `tui/.../themes.py`).
    pub const TETANUS: Colors = Colors {
        primary: Rgb::hex(0xFF9000),
        accent: Rgb::hex(0xFF5FD2),
        foreground: Rgb::hex(0xFFFFFF),
        background: Rgb::hex(0x020003),
        surface: Rgb::hex(0x050109),
        panel: Rgb::hex(0x0B0314),
        success: Rgb::hex(0x4DFFA0),
        warning: Rgb::hex(0xFFE000),
        error: Rgb::hex(0xFF4D6A),
        dark: true,
    };

    /// From the JSON the TUI passes. Every colour must be there; `dark`
    /// is worked out from the colours when it is not.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let color = |key: &str| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .and_then(Rgb::parse)
                .ok_or_else(|| format!("no colour for {key}"))
        };
        let (foreground, background) = (color("foreground")?, color("background")?);
        Ok(Colors {
            primary: color("primary")?,
            accent: color("accent")?,
            foreground,
            background,
            surface: color("surface")?,
            panel: color("panel")?,
            success: color("success")?,
            warning: color("warning")?,
            error: color("error")?,
            dark: value
                .get("dark")
                .and_then(|v| v.as_bool())
                .unwrap_or(foreground.luminance() > background.luminance()),
        })
    }
}

/// What to draw with (`0x00RRGGBB`, as the window's buffer takes them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The window; text fields and cards, which are cut into a surface.
    pub window: u32,
    /// Behind the remote picture, where it does not cover its area.
    pub backdrop: u32,
    /// The header, the panel, the footer, menus and dialogs.
    pub surface: u32,
    /// The tab rail.
    pub rail: u32,
    /// Buttons and select boxes.
    pub control: u32,
    /// Hairlines, a control's edge, a bar's empty part.
    pub line: u32,
    pub text: u32,
    /// Text under the pointer, and the machine's name.
    pub bright: u32,
    /// Labels, icons, tabs that are not open.
    pub muted: u32,
    /// Headings and the open tab.
    pub heading: u32,
    /// The edge of what is under the pointer or has the keyboard.
    pub focus: u32,
    /// The picked item of a list.
    pub selected: u32,
    /// A healthy bar and the session's dot; the same as text.
    pub ok: u32,
    pub ok_text: u32,
    /// A bar getting full, and warnings.
    pub warn: u32,
    /// A full bar.
    pub danger: u32,
    /// What ends the session, and bad news: its text, fill and edge.
    pub danger_text: u32,
    pub danger_fill: u32,
    pub danger_line: u32,
    /// The tile behind the mark. The brand's, whatever the theme.
    pub mark: Rgb,
}

/// A colour as the window's buffer takes it.
pub fn pixel(c: Rgb) -> u32 {
    u32::from(c.0) << 16 | u32::from(c.1) << 8 | u32::from(c.2)
}

fn rgb(pixel: u32) -> Rgb {
    Rgb::hex(pixel)
}

/// `a` with `amount` (0-1) of `b` mixed in.
pub fn mix(a: u32, b: u32, amount: f32) -> u32 {
    pixel(rgb(a).mix(rgb(b), amount))
}

/// How far a control that cannot be used fades into what is behind it.
pub const DISABLED_FADE: f32 = 0.55;

impl Theme {
    pub fn new(c: Colors) -> Self {
        let fg = c.foreground;
        // What stands out most from the background.
        let extreme = if c.dark { Rgb::WHITE } else { Rgb::BLACK };
        // Readable on everything it is drawn on. Those all sit on the
        // background's side of the foreground, so moving towards the
        // foreground helps on each.
        let on_all = |color: Rgb, ratio: f32| {
            [c.surface, c.background, c.panel]
                .into_iter()
                .fold(color, |color, on| color.readable_on(on, ratio, fg))
        };
        let danger_fill = c.surface.mix(c.error, 0.15);
        // As much of the primary colour as leaves the text on it readable.
        let selected = [0.25, 0.18, 0.12, 0.06]
            .into_iter()
            .map(|amount| c.surface.mix(c.primary, amount))
            .find(|on| fg.contrast(*on) >= 4.5)
            .unwrap_or(c.surface);
        Theme {
            window: pixel(c.background),
            backdrop: pixel(if c.dark {
                c.background.mix(Rgb::BLACK, 0.5)
            } else {
                c.background.mix(fg, 0.12)
            }),
            surface: pixel(c.surface),
            rail: pixel(c.background.mix(c.surface, 0.5)),
            control: pixel(c.panel),
            line: pixel(c.panel.mix(fg, 0.18)),
            text: pixel(fg),
            bright: pixel(fg.mix(extreme, 0.6)),
            muted: pixel(on_all(fg.mix(c.surface, 0.45), 4.5)),
            heading: pixel(on_all(c.accent, 4.5)),
            focus: pixel(on_all(c.primary, 3.0)),
            selected: pixel(selected),
            ok: pixel(on_all(c.success, 3.0)),
            ok_text: pixel(on_all(c.success, 4.5)),
            warn: pixel(on_all(c.warning, 4.5)),
            danger: pixel(on_all(c.error, 3.0)),
            danger_text: pixel(c.error.mix(fg, 0.25).readable_on(danger_fill, 4.5, fg)),
            danger_fill: pixel(danger_fill),
            danger_line: pixel(c.surface.mix(c.error, 0.35)),
            mark: palette::RUST_ON_DARK,
        }
    }

    /// The theme the TUI passed in [`THEME_ENV`], else its default one.
    pub fn from_env() -> Self {
        match std::env::var(THEME_ENV) {
            Ok(text) if !text.is_empty() => match Colors::from_json(&text) {
                Ok(colors) => Theme::new(colors),
                Err(e) => {
                    tracing::warn!("ignoring {THEME_ENV}: {e}");
                    Theme::default()
                }
            },
            _ => Theme::default(),
        }
    }

    /// `color` on a control that cannot be used, over `behind`.
    pub fn disabled(&self, color: u32, behind: u32) -> u32 {
        mix(color, behind, DISABLED_FADE)
    }

    /// Every text (or stroke) colour with what it is drawn on and the
    /// least contrast it needs there. For the tests, and for anyone
    /// changing how a colour is worked out. (A theme's own text on its own
    /// backgrounds is as readable as the theme makes it: the low-contrast
    /// ones manage 4.5.)
    pub fn pairs(&self) -> Vec<(&'static str, u32, u32, f32)> {
        vec![
            ("text on a surface", self.text, self.surface, 4.5),
            ("text in a field", self.text, self.window, 4.5),
            ("text on a button", self.text, self.control, 4.5),
            ("text on the picked item", self.text, self.selected, 4.5),
            ("hover text on a button", self.bright, self.control, 4.5),
            ("muted text on a surface", self.muted, self.surface, 4.5),
            ("muted text in a card", self.muted, self.window, 4.5),
            ("an icon on a button", self.muted, self.control, 3.0),
            ("a closed tab on the rail", self.muted, self.rail, 4.5),
            ("a heading", self.heading, self.surface, 4.5),
            ("a focused edge", self.focus, self.surface, 3.0),
            ("the session's state", self.ok_text, self.surface, 4.5),
            ("a healthy bar", self.ok, self.window, 3.0),
            ("a warning", self.warn, self.surface, 4.5),
            ("a full bar", self.danger, self.window, 3.0),
            ("Disconnect", self.danger_text, self.danger_fill, 4.5),
        ]
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::new(Colors::TETANUS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Themes the TUI bundles, as it would pass them: dark and light, low
    /// and high contrast.
    const BUNDLED: [(&str, &str); 5] = [
        (
            "tetanus",
            r##"{"primary":"#FF9000","secondary":"#CF8CFF","accent":"#FF5FD2","foreground":"#FFFFFF",
                "background":"#020003","surface":"#050109","panel":"#0B0314","success":"#4DFFA0",
                "warning":"#FFE000","error":"#FF4D6A","dark":true}"##,
        ),
        (
            "everforest",
            r##"{"primary":"#A7C080","secondary":"#7FBBB3","accent":"#DBBC7F","foreground":"#D3C6AA",
                "background":"#272E33","surface":"#2E383C","panel":"#374145","success":"#A7C080",
                "warning":"#E69875","error":"#E67E80","dark":true}"##,
        ),
        (
            "github-light",
            r##"{"primary":"#0969DA","secondary":"#8250DF","accent":"#BC4C00","foreground":"#1F2328",
                "background":"#FFFFFF","surface":"#F6F8FA","panel":"#EAEEF2","success":"#1A7F37",
                "warning":"#9A6700","error":"#CF222E","dark":false}"##,
        ),
        (
            "green-screen",
            r##"{"primary":"#33FF66","secondary":"#1FA347","accent":"#B6FF8A","foreground":"#7CFF9B",
                "background":"#020A04","surface":"#06150A","panel":"#0B2412","success":"#33FF66",
                "warning":"#D7FF5F","error":"#FF5F5F","dark":true}"##,
        ),
        (
            "high-contrast",
            r##"{"primary":"#FFFF00","secondary":"#00FFFF","accent":"#FF00FF","foreground":"#FFFFFF",
                "background":"#000000","surface":"#000000","panel":"#1A1A1A","success":"#00FF00",
                "warning":"#FFA500","error":"#FF4040","dark":true}"##,
        ),
    ];

    #[test]
    fn the_default_is_the_tuis_default_theme() {
        let colors = Colors::from_json(BUNDLED[0].1).unwrap();
        assert_eq!(colors, Colors::TETANUS);
        let t = Theme::default();
        assert_eq!(t, Theme::new(colors));
        assert_eq!(
            (t.window, t.surface, t.control),
            (0x020003, 0x050109, 0x0B0314)
        );
        assert_eq!((t.text, t.heading, t.focus), (0xFFFFFF, 0xFF5FD2, 0xFF9000));
    }

    #[test]
    fn everything_is_readable_in_the_tuis_themes() {
        for (name, json) in BUNDLED {
            let theme = Theme::new(Colors::from_json(json).unwrap());
            for (what, fg, bg, floor) in theme.pairs() {
                let ratio = rgb(fg).contrast(rgb(bg));
                assert!(
                    ratio >= floor,
                    "{name}: {what}: {fg:06X} on {bg:06X} is {ratio:.2}, needs {floor}"
                );
            }
        }
    }

    #[test]
    fn a_light_theme_stays_light() {
        let theme = Theme::new(Colors::from_json(BUNDLED[2].1).unwrap());
        assert!(rgb(theme.backdrop).luminance() > 0.5);
        assert!(rgb(theme.bright).luminance() < rgb(theme.text).luminance());
        // The mark keeps the brand's tile.
        assert_eq!(theme.mark, Theme::default().mark);
    }

    #[test]
    fn a_theme_missing_a_colour_is_refused() {
        for bad in [
            "",
            "[]",
            r##"{"primary": "#FF9000"}"##,
            &BUNDLED[0].1.replace("#020003", "black"),
        ] {
            assert!(Colors::from_json(bad).is_err(), "{bad}");
        }
        // Without `dark`, the colours say.
        let json = BUNDLED[2].1.replace(r#","dark":false"#, "");
        assert!(!Colors::from_json(&json).unwrap().dark);
        let json = BUNDLED[0].1.replace(r#","dark":true"#, "");
        assert!(Colors::from_json(&json).unwrap().dark);
    }

    #[test]
    fn disabled_controls_fade_into_what_is_behind_them() {
        let t = Theme::default();
        assert_eq!(mix(0xFFFFFF, 0x000000, 0.0), 0xFFFFFF);
        assert_eq!(mix(0xFFFFFF, 0x000000, 1.0), 0x000000);
        let faded = t.disabled(t.text, t.control);
        assert!(rgb(faded).contrast(rgb(t.control)) < rgb(t.text).contrast(rgb(t.control)));
        assert!(rgb(faded).contrast(rgb(t.control)) > 2.0);
    }
}
