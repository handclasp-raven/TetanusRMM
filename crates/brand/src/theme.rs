//! The colours every surface draws with, light and dark.
//!
//! The light theme is the artboards'. They show no dark dialogs, so the
//! dark theme is built from the same sheet: Slate as the body, and the
//! colours the sheet uses for the logo on a dark background (a brighter
//! rust for filled shapes, a pale one for text and icon strokes).
//!
//! A company's accent colour takes Rust's place. The shades around it
//! (hover, pressed, the pale tile behind an icon, the accent as text) are
//! worked out from it, so that they stay readable on either background.
//! Live red and the green of the checks carry meaning and never change.

use crate::color::Rgb;

/// The brand sheet's palette.
pub mod palette {
    use crate::color::Rgb;

    pub const RUST: Rgb = Rgb::hex(0xB5441C);
    pub const RUST_DEEP: Rgb = Rgb::hex(0x8F3412);
    pub const SLATE: Rgb = Rgb::hex(0x1E2328);
    pub const STEEL: Rgb = Rgb::hex(0x5B6670);
    pub const MIST: Rgb = Rgb::hex(0xF3F4F5);
    pub const LIVE: Rgb = Rgb::hex(0xC42B1C);
    /// Rust for filled shapes on a dark background.
    pub const RUST_ON_DARK: Rgb = Rgb::hex(0xC9542A);
    /// Rust for text on a dark background.
    pub const RUST_TEXT_ON_DARK: Rgb = Rgb::hex(0xF08A62);
    /// The tile behind a dialog icon.
    pub const TILE: Rgb = Rgb::hex(0xFBEDE7);
    /// Hairlines on white.
    pub const LINE: Rgb = Rgb::hex(0xE3E6E9);
    /// The tray's status dots, and what is behind them on the sheet.
    pub const TRAY_OK: Rgb = Rgb::hex(0x3CB371);
    pub const TRAY_LIVE: Rgb = Rgb::hex(0xFF5A4E);
    pub const TRAY_DARK: Rgb = Rgb::hex(0x202020);
    /// The offline tray icon: its tile and its mark.
    pub const OFFLINE: Rgb = Rgb::hex(0x5E666D);
    pub const OFFLINE_MARK: Rgb = Rgb::hex(0xC7CCD1);
}

/// Least contrast between white and an accent colour for it to carry a
/// button's label.
pub const MIN_ACCENT_CONTRAST: f32 = 4.0;

/// Least contrast between a filled accent shape and the body behind it.
pub const MIN_FILL_CONTRAST: f32 = 1.7;

/// Whether `accent` can be a company's accent colour: white text must be
/// readable on it. (Anything darker works; the themes lift it where it
/// has to stand out from a dark background.)
pub fn accent_ok(accent: Rgb) -> bool {
    Rgb::WHITE.contrast(accent) >= MIN_ACCENT_CONTRAST
}

/// What to draw with. All of a surface's colours come from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub dark: bool,
    /// A window's body, and its footer (where the buttons are).
    pub body: Rgb,
    pub footer: Rgb,
    /// An inset panel in the body (the scam warning's two points).
    pub panel: Rgb,
    /// Hairlines: above the footer, around secondary buttons and fields.
    pub line: Rgb,
    pub text: Rgb,
    /// Subtitles, labels, hints.
    pub text_muted: Rgb,
    /// Filled accent shapes (the primary button, the countdown bar, the
    /// mark's tile), their label, and the button's other states.
    pub accent: Rgb,
    pub accent_hover: Rgb,
    pub accent_pressed: Rgb,
    pub on_accent: Rgb,
    /// The accent as text, as an icon's stroke or as a thin bar, on `body`.
    pub accent_text: Rgb,
    /// The tile behind a dialog icon.
    pub tile: Rgb,
    /// Secondary buttons and text fields.
    pub control: Rgb,
    pub control_hover: Rgb,
    pub control_line: Rgb,
    /// A progress bar's empty part.
    pub track: Rgb,
    /// Ends a session; a session is live. Never the company's colour.
    pub live: Rgb,
    pub live_hover: Rgb,
    /// `live` as text or a stroke on `body`.
    pub live_text: Rgb,
    /// The check marks of an assurance ("Stays on this computer").
    pub ok: Rgb,
    /// The round initial of a technician.
    pub avatar: Rgb,
    pub on_avatar: Rgb,
}

impl Theme {
    /// The artboards' theme, with `accent` in Rust's place if given.
    pub fn light(accent: Option<Rgb>) -> Self {
        let body = Rgb::WHITE;
        let accent = accent.unwrap_or(palette::RUST);
        Theme {
            dark: false,
            body,
            footer: Rgb::hex(0xF2F2F2),
            panel: Rgb::hex(0xF5F6F8),
            line: Rgb::hex(0xE2E4E7),
            text: Rgb::hex(0x1B1B1B),
            text_muted: palette::STEEL,
            accent,
            accent_hover: accent.mix(Rgb::BLACK, 0.10),
            accent_pressed: accent.mix(Rgb::BLACK, 0.21),
            on_accent: Rgb::WHITE,
            accent_text: accent.readable_on(body, 4.5, Rgb::BLACK),
            tile: body.mix(accent, 0.10),
            control: Rgb::WHITE,
            control_hover: Rgb::hex(0xF6F6F6),
            control_line: Rgb::hex(0xD0D3D7),
            track: Rgb::hex(0xE6E7E9),
            live: palette::LIVE,
            live_hover: palette::LIVE.mix(Rgb::BLACK, 0.10),
            live_text: palette::LIVE,
            ok: Rgb::hex(0x2E7D4F),
            avatar: palette::SLATE,
            on_avatar: Rgb::WHITE,
        }
    }

    /// The dark theme, with `accent` in Rust's place if given.
    pub fn dark(accent: Option<Rgb>) -> Self {
        let body = palette::SLATE;
        // The sheet gives its own rust for dark backgrounds; a company's
        // colour is lifted a little in the same way, while it still
        // carries white text.
        let (fill, text) = match accent {
            None => (palette::RUST_ON_DARK, palette::RUST_TEXT_ON_DARK),
            Some(accent) => {
                // At least far enough that a button of it shows against
                // the body (a navy or black accent would vanish into it).
                let lifted =
                    accent
                        .mix(Rgb::WHITE, 0.10)
                        .readable_on(body, MIN_FILL_CONTRAST, Rgb::WHITE);
                let fill = if accent_ok(lifted) { lifted } else { accent };
                (fill, accent.mix(Rgb::WHITE, 0.35))
            }
        };
        Theme {
            dark: true,
            body,
            footer: Rgb::hex(0x171B1F),
            panel: Rgb::hex(0x272D33),
            line: Rgb::hex(0x343B43),
            text: palette::MIST,
            text_muted: Rgb::hex(0xA3ADB7),
            accent: fill,
            accent_hover: fill.mix(Rgb::WHITE, 0.10),
            accent_pressed: fill.mix(Rgb::BLACK, 0.15),
            on_accent: Rgb::WHITE,
            accent_text: text.readable_on(body, 4.5, Rgb::WHITE),
            tile: body.mix(fill, 0.24),
            control: Rgb::hex(0x2A3037),
            control_hover: Rgb::hex(0x333A42),
            control_line: Rgb::hex(0x464E57),
            track: Rgb::hex(0x343B43),
            live: palette::LIVE,
            live_hover: palette::LIVE.mix(Rgb::WHITE, 0.12),
            live_text: palette::TRAY_LIVE,
            ok: Rgb::hex(0x57C98A),
            avatar: Rgb::hex(0x3A424B),
            on_avatar: Rgb::WHITE,
        }
    }

    /// Light or dark, as the system is set.
    pub fn new(dark: bool, accent: Option<Rgb>) -> Self {
        if dark {
            Self::dark(accent)
        } else {
            Self::light(accent)
        }
    }

    /// Every text (or stroke) colour with what it is drawn on and the
    /// least contrast it needs there. For the tests, and for anyone
    /// changing a colour.
    pub fn pairs(&self) -> Vec<(&'static str, Rgb, Rgb, f32)> {
        vec![
            ("text on body", self.text, self.body, 7.0),
            ("text on panel", self.text, self.panel, 7.0),
            ("text on footer", self.text, self.footer, 7.0),
            ("muted text on body", self.text_muted, self.body, 4.5),
            ("muted text on footer", self.text_muted, self.footer, 4.5),
            ("primary button label", self.on_accent, self.accent, 4.0),
            (
                "primary button label, hover",
                self.on_accent,
                self.accent_hover,
                3.5,
            ),
            ("accent text on body", self.accent_text, self.body, 4.5),
            ("icon on its tile", self.accent_text, self.tile, 3.0),
            ("secondary button label", self.text, self.control, 7.0),
            (
                "secondary button label, hover",
                self.text,
                self.control_hover,
                7.0,
            ),
            ("end-session label", Rgb::WHITE, self.live, 4.5),
            ("live text on body", self.live_text, self.body, 4.5),
            ("check on body", self.ok, self.body, 4.5),
            ("avatar initial", self.on_avatar, self.avatar, 7.0),
            ("accent bar on its track", self.accent_text, self.track, 3.0),
            (
                "primary button against the body",
                self.accent,
                self.body,
                MIN_FILL_CONTRAST,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accents a company might pick, from pale-ish to nearly black.
    const ACCENTS: [u32; 9] = [
        0xB5441C, 0x0B5CAD, 0x001F5B, 0x1A7F4B, 0x6B21A8, 0x8A6A00, 0xC2185B, 0x111111, 0x00796B,
    ];

    #[test]
    fn the_light_theme_is_the_artboards() {
        let t = Theme::light(None);
        assert_eq!(t.accent, palette::RUST);
        assert_eq!(t.accent_text, palette::RUST);
        assert_eq!(t.accent_pressed, Rgb(0x8F, 0x36, 0x16));
        assert!(t.tile.contrast(palette::TILE) < 1.02, "{:?}", t.tile);
        assert_eq!(t.avatar, palette::SLATE);
        assert!(!t.dark && Theme::new(false, None) == t);
    }

    #[test]
    fn the_dark_theme_uses_the_sheets_on_dark_rust() {
        let t = Theme::dark(None);
        assert_eq!(t.body, palette::SLATE);
        assert_eq!(t.accent, palette::RUST_ON_DARK);
        assert_eq!(t.accent_text, palette::RUST_TEXT_ON_DARK);
        assert!(t.dark && Theme::new(true, None) == t);
    }

    #[test]
    fn everything_is_readable_in_both_themes_and_any_accepted_accent() {
        let accents = std::iter::once(None).chain(ACCENTS.map(Rgb::hex).map(Some));
        for accent in accents {
            if let Some(accent) = accent {
                assert!(accent_ok(accent), "{accent:?}");
            }
            for theme in [Theme::light(accent), Theme::dark(accent)] {
                for (what, fg, bg, floor) in theme.pairs() {
                    let ratio = fg.contrast(bg);
                    assert!(
                        ratio >= floor,
                        "{what}: {} on {} is {ratio:.2}, needs {floor} (dark: {}, accent {accent:?})",
                        fg.to_hex(),
                        bg.to_hex(),
                        theme.dark
                    );
                }
            }
        }
    }

    #[test]
    fn accents_too_light_for_white_text_are_refused() {
        for light in [0xFFFFFF, 0xFFD400, 0x7FD1FF, 0xF08A62, 0x9AD14B] {
            assert!(!accent_ok(Rgb::hex(light)), "{light:06X}");
        }
        assert!(accent_ok(palette::RUST) && accent_ok(palette::RUST_ON_DARK));
    }

    #[test]
    fn live_and_ok_never_take_the_accent() {
        let blue = Some(Rgb::hex(0x0B5CAD));
        for (plain, branded) in [
            (Theme::light(None), Theme::light(blue)),
            (Theme::dark(None), Theme::dark(blue)),
        ] {
            assert_eq!(plain.live, branded.live);
            assert_eq!(plain.live_text, branded.live_text);
            assert_eq!(plain.ok, branded.ok);
            assert_ne!(plain.accent, branded.accent);
        }
    }
}
