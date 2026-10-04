//! Writes the brand's drawings to a directory, to look at them beside the
//! brand sheet: `cargo run -p brand --example sheet -- <dir>`.
//!
//! - `app.ico`: the application icon, every size;
//! - `tray-<state>-<light|dark>.ico`: the tray icons at 16, 24 and 32;
//! - `sheet.svg`: the mark, the icons and both themes' colours.

use std::path::PathBuf;

use brand::mark::TrayState;
use brand::theme::palette;
use brand::{ico, icons, raster, svg, Rgb, Theme};

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("app.ico"),
        ico::encode(&raster::app_icons(palette::RUST)),
    )?;
    for (state, name) in [
        (TrayState::Connected, "connected"),
        (TrayState::Live, "live"),
        (TrayState::Offline, "offline"),
    ] {
        for (dark, taskbar) in [(true, "dark"), (false, "light")] {
            let tile = raster::tray_tile(dark, None);
            let images: Vec<_> = [16, 24, 32, 64]
                .into_iter()
                .map(|size| raster::tray_icon(size, state, tile))
                .collect();
            std::fs::write(
                dir.join(format!("tray-{name}-{taskbar}.ico")),
                ico::encode(&images),
            )?;
        }
    }

    let mut body = String::new();
    for (row, accent) in [None, Some(Rgb::hex(0x0B5CAD))].into_iter().enumerate() {
        for (col, theme) in [Theme::light(accent), Theme::dark(accent)]
            .into_iter()
            .enumerate()
        {
            let (x, y) = (col * 620, row * 250);
            body += &format!(
                "<g transform=\"translate({x} {y})\"><rect width=\"620\" height=\"250\" fill=\"{}\"/>",
                theme.body.to_hex()
            );
            body += &format!(
                "<g transform=\"translate(20 20)\">{}</g>",
                svg::mark(48, theme.accent)
            );
            for (i, icon) in icons::ALL.iter().enumerate() {
                let (ix, iy) = (90 + (i % 7) * 72, 20 + (i / 7) * 72);
                body += &format!(
                    "<g transform=\"translate({ix} {iy})\"><rect width=\"56\" height=\"56\" rx=\"12\" \
                     fill=\"{}\"/><g transform=\"translate(14 14)\" color=\"{}\">{}</g></g>",
                    theme.tile.to_hex(),
                    theme.accent_text.to_hex(),
                    svg::icon(icon, 28)
                );
            }
            let swatches = [
                theme.accent,
                theme.accent_hover,
                theme.accent_pressed,
                theme.accent_text,
                theme.tile,
                theme.footer,
                theme.panel,
                theme.control,
                theme.line,
                theme.track,
                theme.text,
                theme.text_muted,
                theme.live,
                theme.live_text,
                theme.ok,
                theme.avatar,
            ];
            for (i, c) in swatches.into_iter().enumerate() {
                body += &format!(
                    "<rect x=\"{}\" y=\"180\" width=\"32\" height=\"32\" rx=\"6\" fill=\"{}\" \
                     stroke=\"{}\"/>",
                    20 + i * 36,
                    c.to_hex(),
                    theme.line.to_hex()
                );
            }
            body += "</g>";
        }
    }
    let sheet = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1240\" height=\"500\">{body}</svg>"
    );
    std::fs::write(dir.join("sheet.svg"), sheet)
}
