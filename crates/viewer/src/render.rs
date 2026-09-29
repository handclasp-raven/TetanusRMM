//! Drawing into the window's pixel buffer: the remote picture scaled to fit,
//! and a text overlay (monitor picker, status) using an 8x8 bitmap font, so
//! the viewer needs no GUI toolkit.

use font8x8::UnicodeFonts;
use protocol::media::MonitorInfo;

pub const BACKGROUND: u32 = 0x0010_1010;

/// Where a `src_w x src_h` picture goes in a `dst_w x dst_h` window,
/// scaled to fit with its aspect ratio kept: `(x, y, w, h)`.
pub fn fit(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (u32, u32, u32, u32) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return (0, 0, 0, 0);
    }
    let (sw, sh, dw, dh) = (
        u64::from(src_w),
        u64::from(src_h),
        u64::from(dst_w),
        u64::from(dst_h),
    );
    let (w, h) = if sw * dh > sh * dw {
        (dw, (sh * dw / sw).max(1))
    } else {
        ((sw * dh / sh).max(1), dh)
    };
    (
        ((dw - w) / 2) as u32,
        ((dh - h) / 2) as u32,
        w as u32,
        h as u32,
    )
}

/// Inverse of [`fit`]: the picture pixel under window position `(x, y)`
/// (physical pixels), or `None` in the letterbox bars.
pub fn unfit(
    (x, y): (f64, f64),
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Option<(u32, u32)> {
    let (ox, oy, w, h) = fit(src_w, src_h, dst_w, dst_h);
    let (rx, ry) = (x - f64::from(ox), y - f64::from(oy));
    if w == 0 || rx < 0.0 || ry < 0.0 || rx >= f64::from(w) || ry >= f64::from(h) {
        return None;
    }
    // Same nearest-neighbour mapping as `draw_picture`.
    let px = (rx.floor() as u64 * u64::from(src_w) / u64::from(w)) as u32;
    let py = (ry.floor() as u64 * u64::from(src_h) / u64::from(h)) as u32;
    Some((px.min(src_w - 1), py.min(src_h - 1)))
}

/// Scale `src` into `dst` (nearest neighbour), letterboxed on `BACKGROUND`.
pub fn draw_picture(src: &[u32], src_w: u32, src_h: u32, dst: &mut [u32], dst_w: u32, dst_h: u32) {
    dst.fill(BACKGROUND);
    let (ox, oy, w, h) = fit(src_w, src_h, dst_w, dst_h);
    if w == 0 {
        return;
    }
    let xs: Vec<usize> = (0..w)
        .map(|x| (u64::from(x) * u64::from(src_w) / u64::from(w)) as usize)
        .collect();
    for y in 0..h {
        let sy = (u64::from(y) * u64::from(src_h) / u64::from(h)) as usize;
        let src_row = &src[sy * src_w as usize..][..src_w as usize];
        let dst_row = &mut dst[((oy + y) * dst_w + ox) as usize..][..w as usize];
        for (d, &sx) in dst_row.iter_mut().zip(&xs) {
            *d = src_row[sx];
        }
    }
}

/// A `width x height` pixel buffer to draw into (`0x00RRGGBB`).
pub struct Canvas<'a> {
    pub pixels: &'a mut [u32],
    pub width: u32,
    pub height: u32,
}

impl Canvas<'_> {
    fn put(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            self.pixels[(y * self.width + x) as usize] = color;
        }
    }
}

/// Draw `text` with its top-left at `(x, y)`, each font pixel `scale` wide.
pub fn draw_text(canvas: &mut Canvas, (x, y): (u32, u32), scale: u32, color: u32, text: &str) {
    for (i, ch) in text.chars().enumerate() {
        let glyph = font8x8::BASIC_FONTS
            .get(ch)
            .or_else(|| font8x8::BASIC_FONTS.get('?'));
        let Some(glyph) = glyph else { continue };
        let gx = x + i as u32 * 8 * scale;
        for (row, bits) in glyph.iter().enumerate() {
            for col in (0..8).filter(|col| bits & (1 << col) != 0) {
                for dy in 0..scale {
                    for dx in 0..scale {
                        canvas.put(gx + col * scale + dx, y + row as u32 * scale + dy, color);
                    }
                }
            }
        }
    }
}

/// Fill the rectangle `(x, y, w, h)`, clipped to the canvas.
fn fill_rect(canvas: &mut Canvas, (x, y, w, h): (u32, u32, u32, u32), color: u32) {
    for py in y..(y + h).min(canvas.height) {
        for px in x..(x + w).min(canvas.width) {
            canvas.put(px, py, color);
        }
    }
}

/// The lines of the monitor picker.
pub fn picker_lines(monitors: &[MonitorInfo], active: Option<u32>) -> Vec<String> {
    let mut lines = vec!["Select monitor (press number, Tab to close):".to_owned()];
    if monitors.is_empty() {
        lines.push("  (waiting for monitor list)".to_owned());
    }
    for (i, m) in monitors.iter().enumerate().take(9) {
        lines.push(format!(
            "{} {}: {} {}x{}{}",
            if Some(m.id) == active { ">" } else { " " },
            i + 1,
            m.name,
            m.width,
            m.height,
            if m.primary { " (primary)" } else { "" }
        ));
    }
    lines
}

/// Draw the picker as a panel in the top-left corner.
pub fn draw_picker(canvas: &mut Canvas, monitors: &[MonitorInfo], active: Option<u32>) {
    let lines = picker_lines(monitors, active);
    let scale = 2;
    let line_h = 8 * scale + 6;
    let width = lines
        .iter()
        .map(|l| l.chars().count() as u32)
        .max()
        .unwrap_or(0)
        * 8
        * scale
        + 24;
    let height = lines.len() as u32 * line_h + 18;
    fill_rect(canvas, (10, 10, width, height), 0x0020_2833);
    for (i, line) in lines.iter().enumerate() {
        let color = if i == 0 { 0x00ff_d27a } else { 0x00e8_e8e8 };
        draw_text(canvas, (22, 20 + i as u32 * line_h), scale, color, line);
    }
}

/// A one-line status message centred in the window (connecting, closed...).
pub fn draw_status(canvas: &mut Canvas, text: &str) {
    let scale = 2;
    let w = text.chars().count() as u32 * 8 * scale;
    let x = canvas.width.saturating_sub(w) / 2;
    let y = canvas.height.saturating_sub(8 * scale) / 2;
    draw_text(canvas, (x, y), scale, 0x00e8_e8e8, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfit_finds_the_picture_pixel_under_the_cursor() {
        // 1920x1080 in a 1000x1000 window: letterboxed top and bottom.
        let (ox, oy, w, h) = fit(1920, 1080, 1000, 1000);
        assert_eq!((ox, oy, w, h), (0, 219, 1000, 562));
        assert_eq!(unfit((0.0, 219.0), 1920, 1080, 1000, 1000), Some((0, 0)));
        assert_eq!(
            unfit((999.9, 780.9), 1920, 1080, 1000, 1000),
            Some((1918, 1078))
        );
        assert_eq!(unfit((500.0, 100.0), 1920, 1080, 1000, 1000), None, "bar");
        assert_eq!(unfit((500.0, 781.0), 1920, 1080, 1000, 1000), None, "bar");
        assert_eq!(unfit((-1.0, 500.0), 1920, 1080, 1000, 1000), None);
    }

    #[test]
    fn unfit_agrees_with_draw_picture() {
        // Each pixel's colour encodes its position; what is drawn at a window
        // position must be the pixel unfit reports for it.
        let (sw, sh, dw, dh) = (37u32, 23u32, 101u32, 77u32);
        let src: Vec<u32> = (0..sw * sh).collect();
        let mut dst = vec![0u32; (dw * dh) as usize];
        draw_picture(&src, sw, sh, &mut dst, dw, dh);
        for y in 0..dh {
            for x in 0..dw {
                let drawn = dst[(y * dw + x) as usize];
                match unfit((f64::from(x) + 0.5, f64::from(y) + 0.5), sw, sh, dw, dh) {
                    Some((px, py)) => assert_eq!(drawn, py * sw + px, "at {x},{y}"),
                    None => assert_eq!(drawn, BACKGROUND, "at {x},{y}"),
                }
            }
        }
    }

    #[test]
    fn fit_keeps_aspect_ratio_and_centres() {
        assert_eq!(fit(2560, 1440, 1280, 720), (0, 0, 1280, 720));
        // Wider window: pillarbox.
        assert_eq!(fit(1600, 1200, 1600, 900), (200, 0, 1200, 900));
        // Taller window: letterbox.
        assert_eq!(fit(1920, 1080, 960, 1080), (0, 270, 960, 540));
        assert_eq!(fit(0, 10, 100, 100), (0, 0, 0, 0));
    }

    #[test]
    fn scaling_samples_the_right_source_pixels() {
        // 2x2 source: four distinct colours, drawn into 4x4.
        let src = [1, 2, 3, 4];
        let mut dst = vec![0; 16];
        draw_picture(&src, 2, 2, &mut dst, 4, 4);
        assert_eq!(dst, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    #[test]
    fn letterbox_area_is_background() {
        let src = [7; 4];
        let mut dst = vec![0; 4 * 2];
        draw_picture(&src, 2, 2, &mut dst, 4, 2);
        assert_eq!(
            dst,
            [BACKGROUND, 7, 7, BACKGROUND, BACKGROUND, 7, 7, BACKGROUND]
        );
    }

    #[test]
    fn text_is_drawn_and_clipped() {
        let mut dst = vec![0; 16 * 8];
        let mut canvas = Canvas {
            pixels: &mut dst,
            width: 16,
            height: 8,
        };
        draw_text(&mut canvas, (0, 0), 1, 0xff, "II");
        // Off-screen text must not panic.
        draw_text(&mut canvas, (12, 4), 3, 0xff, "clipped");
        assert!(dst.contains(&0xff));
    }

    #[test]
    fn picker_marks_the_active_monitor() {
        let m = |id, primary| MonitorInfo {
            id,
            name: format!(r"\\.\DISPLAY{}", id + 1),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary,
        };
        let lines = picker_lines(&[m(0, true), m(1, false)], Some(1));
        assert_eq!(lines[1], r"  1: \\.\DISPLAY1 1920x1080 (primary)");
        assert_eq!(lines[2], r"> 2: \\.\DISPLAY2 1920x1080");
        assert!(picker_lines(&[], None)[1].contains("waiting"));
    }
}
