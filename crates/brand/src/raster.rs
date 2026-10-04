//! The mark as pixels: the application icon and the tray icons, at any
//! size, antialiased by supersampling. Also what a company's own logo
//! needs to stand in for the mark in the tray: scaling to size, greying
//! out when offline, and the status badge.

use crate::color::Rgb;
use crate::mark::{Mark, TrayState, Variant, BADGE_DOT, BADGE_HOLE, BOX};
use crate::path::Point;
use crate::theme::palette;

/// A square picture, `size` pixels a side: RGBA, alpha not premultiplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub size: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.size + x) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }
}

enum Shape {
    RoundRect(f32, f32, f32, f32, f32),
    Polygon(Vec<Point>),
    Circle(f32, f32, f32),
}

impl Shape {
    fn contains(&self, (px, py): Point) -> bool {
        match self {
            Shape::RoundRect(x, y, w, h, r) => {
                if px < *x || py < *y || px > x + w || py > y + h {
                    return false;
                }
                // Inside the corners' squares, the corner's circle decides.
                let cx = px.clamp(x + r, x + w - r);
                let cy = py.clamp(y + r, y + h - r);
                (px - cx).powi(2) + (py - cy).powi(2) <= r * r
            }
            Shape::Polygon(points) => {
                // Even-odd: count the edges a ray to the right crosses.
                let mut inside = false;
                let mut prev = points[points.len() - 1];
                for &(x, y) in points {
                    if (y > py) != (prev.1 > py) {
                        let at = x + (py - y) / (prev.1 - y) * (prev.0 - x);
                        if px < at {
                            inside = !inside;
                        }
                    }
                    prev = (x, y);
                }
                inside
            }
            Shape::Circle(cx, cy, r) => (px - cx).powi(2) + (py - cy).powi(2) <= r * r,
        }
    }
}

enum Paint {
    Fill(Rgb),
    /// Cut through to transparent.
    Clear,
}

/// Draw `layers`, given in the mark's 64-unit box, `size` pixels square.
fn render(size: u32, layers: &[(Shape, Paint)]) -> Image {
    // More samples where a pixel covers more of the drawing.
    let n: u32 = if size <= 64 { 8 } else { 4 };
    let scale = BOX / size as f32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // Premultiplied sums over the samples.
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..n {
                for sx in 0..n {
                    let point = (
                        (x as f32 + (sx as f32 + 0.5) / n as f32) * scale,
                        (y as f32 + (sy as f32 + 0.5) / n as f32) * scale,
                    );
                    let mut colour = None;
                    for (shape, paint) in layers {
                        if shape.contains(point) {
                            colour = match paint {
                                Paint::Fill(c) => Some(*c),
                                Paint::Clear => None,
                            };
                        }
                    }
                    if let Some(c) = colour {
                        r += u32::from(c.0);
                        g += u32::from(c.1);
                        b += u32::from(c.2);
                        a += 1;
                    }
                }
            }
            if a == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let ch = |sum: u32| ((sum + a / 2) / a) as u8;
                let alpha = ((a * 255 + n * n / 2) / (n * n)) as u8;
                rgba.extend_from_slice(&[ch(r), ch(g), ch(b), alpha]);
            }
        }
    }
    Image { size, rgba }
}

fn mark_layers(variant: Variant, tile: Rgb, mark: Rgb) -> Vec<(Shape, Paint)> {
    let m = Mark::of(variant);
    let (x, y, w, h, r) = m.bar;
    vec![
        (
            Shape::RoundRect(0.0, 0.0, BOX, BOX, m.tile_radius),
            Paint::Fill(tile),
        ),
        (Shape::RoundRect(x, y, w, h, r), Paint::Fill(mark)),
        (Shape::Polygon(m.stem.to_vec()), Paint::Fill(mark)),
    ]
}

/// The application icon, `size` pixels square: the white mark on a tile
/// of `tile` (Rust, unless a company's accent takes its place).
pub fn app_icon(size: u32, tile: Rgb) -> Image {
    render(
        size,
        &mark_layers(Variant::for_size(size), tile, Rgb::WHITE),
    )
}

/// The sizes an `.ico` carries.
pub const ICON_SIZES: [u32; 5] = [16, 24, 32, 48, 256];

/// The application icon at every size an `.ico` carries.
pub fn app_icons(tile: Rgb) -> Vec<Image> {
    ICON_SIZES
        .iter()
        .map(|&size| app_icon(size, tile))
        .collect()
}

fn badge_colour(state: TrayState) -> Option<Rgb> {
    match state {
        TrayState::Connected => Some(palette::TRAY_OK),
        TrayState::Live => Some(palette::TRAY_LIVE),
        TrayState::Offline => None,
    }
}

/// The tray icon for `state`, `size` pixels square. `tile` is the accent
/// to draw the mark on (see [`tray_tile`]); offline is grey whatever it
/// is. The badge sits in a hole cut through the icon, so the taskbar
/// shows around it.
pub fn tray_icon(size: u32, state: TrayState, tile: Rgb) -> Image {
    // The tray is small: always the heaviest drawing, as on the sheet.
    let mut layers = match state {
        TrayState::Offline => mark_layers(Variant::Small, palette::OFFLINE, palette::OFFLINE_MARK),
        _ => mark_layers(Variant::Small, tile, Rgb::WHITE),
    };
    if let Some(dot) = badge_colour(state) {
        let (hx, hy, hr) = BADGE_HOLE;
        let (dx, dy, dr) = BADGE_DOT;
        layers.push((Shape::Circle(hx, hy, hr), Paint::Clear));
        layers.push((Shape::Circle(dx, dy, dr), Paint::Fill(dot)));
    }
    render(size, &layers)
}

/// The tile colour of the tray icon: the sheet's brighter rust on a dark
/// taskbar, Rust on a light one; a company's accent in either's place.
pub fn tray_tile(dark_taskbar: bool, accent: Option<Rgb>) -> Rgb {
    crate::theme::Theme::new(dark_taskbar, accent).accent
}

/// `image` (any size, RGBA rows of `width`) scaled to `size` pixels
/// square, keeping its shape and centred: a company's logo, for the tray
/// and the dialogs' corner.
pub fn fit(rgba: &[u8], width: u32, height: u32, size: u32) -> Image {
    let mut out = vec![0u8; (size * size * 4) as usize];
    if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
        return Image { size, rgba: out };
    }
    let scale = (size as f32 / width as f32).min(size as f32 / height as f32);
    let (w, h) = (
        ((width as f32 * scale).round() as u32).clamp(1, size),
        ((height as f32 * scale).round() as u32).clamp(1, size),
    );
    let (ox, oy) = ((size - w) / 2, (size - h) / 2);
    for y in 0..h {
        for x in 0..w {
            // Average the source pixels this one covers (premultiplied).
            let x0 = x * width / w;
            let x1 = (((x + 1) * width).div_ceil(w)).clamp(x0 + 1, width);
            let y0 = y * height / h;
            let y1 = (((y + 1) * height).div_ceil(h)).clamp(y0 + 1, height);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * width + sx) * 4) as usize;
                    let alpha = u64::from(rgba[i + 3]);
                    r += u64::from(rgba[i]) * alpha;
                    g += u64::from(rgba[i + 1]) * alpha;
                    b += u64::from(rgba[i + 2]) * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            let i = (((oy + y) * size + ox + x) * 4) as usize;
            // Nothing but transparent pixels leaves this one transparent.
            if let (Some(r), Some(g), Some(b)) =
                (r.checked_div(a), g.checked_div(a), b.checked_div(a))
            {
                out[i..i + 4].copy_from_slice(&[r as u8, g as u8, b as u8, (a / n) as u8]);
            }
        }
    }
    Image { size, rgba: out }
}

/// Put the status badge of `state` on `image` (a company's logo in the
/// tray): the same hole and dot as on the mark. Offline greys the image
/// instead, as the sheet's offline icon has no badge.
pub fn badge(image: &mut Image, state: TrayState) {
    let Some(dot) = badge_colour(state) else {
        for px in image.rgba.as_chunks_mut::<4>().0 {
            let grey = Rgb(px[0], px[1], px[2]).luminance().sqrt();
            // Towards the offline tile's grey, keeping some of the shape.
            let level = (64.0 + grey * 150.0) as u8;
            px[..3].copy_from_slice(&[level, level, level]);
            px[3] = (u16::from(px[3]) * 4 / 5) as u8;
        }
        return;
    };
    let size = image.size;
    let hole = render(
        size,
        &[(
            Shape::Circle(BADGE_HOLE.0, BADGE_HOLE.1, BADGE_HOLE.2),
            Paint::Fill(Rgb::BLACK),
        )],
    );
    let spot = render(
        size,
        &[(
            Shape::Circle(BADGE_DOT.0, BADGE_DOT.1, BADGE_DOT.2),
            Paint::Fill(dot),
        )],
    );
    for i in 0..(size * size) as usize {
        let px = &mut image.rgba[i * 4..i * 4 + 4];
        let cut = u16::from(hole.rgba[i * 4 + 3]);
        px[3] = (u16::from(px[3]) * (255 - cut) / 255) as u8;
        let cover = u16::from(spot.rgba[i * 4 + 3]);
        if cover > 0 {
            // The dot goes in the hole, where nothing else is left.
            px[..3].copy_from_slice(&spot.rgba[i * 4..i * 4 + 3]);
            px[3] = px[3].max(cover as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: [u8; 4] = [0xB5, 0x44, 0x1C, 0xFF];
    const WHITE: [u8; 4] = [0xFF; 4];

    #[test]
    fn the_app_icon_is_the_white_nail_on_a_rust_tile() {
        let icon = app_icon(256, palette::RUST);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        // Corners are cut away; the tile is solid inside them.
        assert_eq!(icon.pixel(0, 0)[3], 0);
        assert_eq!(icon.pixel(255, 255)[3], 0);
        assert_eq!(icon.pixel(128, 12), RUST);
        // The crossbar (y 13-22 of 64) and the stem under it are white...
        assert_eq!(icon.pixel(128, 70), WHITE);
        assert_eq!(icon.pixel(70, 70), WHITE);
        assert_eq!(icon.pixel(128, 150), WHITE);
        // ...the stem is narrower than the bar, and ends in a point.
        assert_eq!(icon.pixel(70, 150), RUST);
        assert_eq!(icon.pixel(128, 204), WHITE);
        assert_eq!(icon.pixel(118, 204), RUST);
        assert_eq!(icon.pixel(128, 215), RUST);
    }

    #[test]
    fn every_size_is_drawn_and_edges_are_soft() {
        let icons = app_icons(palette::RUST);
        assert_eq!(icons.iter().map(|i| i.size).collect::<Vec<_>>(), ICON_SIZES);
        for icon in &icons {
            let alphas: Vec<u8> = icon.rgba.as_chunks::<4>().0.iter().map(|p| p[3]).collect();
            assert!(alphas.contains(&255), "{}", icon.size);
            // Antialiased: some pixels at the rounded corners are partial.
            assert!(alphas.iter().any(|a| (1..255).contains(a)), "{}", icon.size);
            // Mostly tile, with a clear white mark.
            let white = icon
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| **p == WHITE)
                .count();
            let share = white as f32 / (icon.size * icon.size) as f32;
            assert!((0.08..0.35).contains(&share), "{}: {share}", icon.size);
        }
        // The 16 px drawing is the heavy one: more of it is white.
        let share = |i: &Image| {
            i.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] > 230 && p[2] > 230)
                .count() as f32
                / (i.size * i.size) as f32
        };
        assert!(share(&icons[0]) > share(&icons[4]));
    }

    #[test]
    fn tray_icons_show_their_state() {
        let tile = tray_tile(true, None);
        assert_eq!(tile, palette::RUST_ON_DARK);
        assert_eq!(tray_tile(false, None), palette::RUST);
        assert_eq!(
            tray_tile(false, Some(Rgb::hex(0x0B5CAD))),
            Rgb::hex(0x0B5CAD)
        );
        let at = |state| tray_icon(64, state, tile);
        // The badge's centre is the state's colour...
        assert_eq!(
            at(TrayState::Connected).pixel(52, 52),
            [0x3C, 0xB3, 0x71, 0xFF]
        );
        assert_eq!(at(TrayState::Live).pixel(52, 52), [0xFF, 0x5A, 0x4E, 0xFF]);
        // ...in a ring cut through to the taskbar.
        assert_eq!(at(TrayState::Live).pixel(52, 42)[3], 0);
        assert_eq!(at(TrayState::Live).pixel(30, 5), [0xC9, 0x54, 0x2A, 0xFF]);
        // Offline: grey, no badge.
        let offline = at(TrayState::Offline);
        assert_eq!(offline.pixel(52, 52), [0x5E, 0x66, 0x6D, 0xFF]);
        assert_eq!(offline.pixel(32, 15), [0xC7, 0xCC, 0xD1, 0xFF]);
    }

    #[test]
    fn a_logo_is_fitted_greyed_and_badged() {
        // A wide blue logo, 8x4, into 16x16: letterboxed above and below.
        let logo: Vec<u8> = [0x0B, 0x5C, 0xAD, 0xFF].repeat(8 * 4);
        let fitted = fit(&logo, 8, 4, 16);
        assert_eq!(fitted.pixel(8, 8), [0x0B, 0x5C, 0xAD, 0xFF]);
        assert_eq!(fitted.pixel(8, 1)[3], 0);
        assert_eq!(fitted.pixel(8, 14)[3], 0);
        // Shrinking averages: a 2x2 checker of black and white is grey.
        let checker = [[0u8, 0, 0, 255], [255, 255, 255, 255]];
        let src: Vec<u8> = (0..16).flat_map(|i| checker[(i % 4 + i / 4) % 2]).collect();
        assert_eq!(fit(&src, 4, 4, 2).pixel(0, 0), [127, 127, 127, 255]);
        // Nothing to fit is an empty picture, not a panic.
        assert!(fit(&[], 0, 0, 16).rgba.iter().all(|b| *b == 0));

        let square: Vec<u8> = [0x0B, 0x5C, 0xAD, 0xFF].repeat(64 * 64);
        let mut live = fit(&square, 64, 64, 64);
        badge(&mut live, TrayState::Live);
        assert_eq!(live.pixel(52, 52), [0xFF, 0x5A, 0x4E, 0xFF]);
        assert_eq!(live.pixel(52, 42)[3], 0);
        assert_eq!(live.pixel(10, 10), [0x0B, 0x5C, 0xAD, 0xFF]);
        let mut offline = fit(&square, 64, 64, 64);
        badge(&mut offline, TrayState::Offline);
        let px = offline.pixel(10, 10);
        assert!(px[0] == px[1] && px[1] == px[2] && px[3] < 255, "{px:?}");
    }
}
