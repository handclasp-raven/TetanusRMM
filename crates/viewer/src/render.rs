//! Drawing into the window's pixel buffer: the remote picture placed in
//! its area (scaled to fit, stretched, or filling it), and text and boxes
//! for the viewer's own controls using an 8x8 bitmap font, so the viewer
//! needs no GUI toolkit. Text can be any size: whole multiples of 8 pixels
//! are drawn crisp, other sizes are scaled from the bitmap with
//! area-averaging (anti-aliased).

use font8x8::UnicodeFonts;

pub const BACKGROUND: u32 = 0x0010_1010;

/// A rectangle in window pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, (x, y): (f64, f64)) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x + self.w)
            && y < f64::from(self.y + self.h)
    }

    pub fn right(&self) -> u32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }
}

/// How the remote picture fills its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    /// As large as fits, aspect ratio kept (bars at the sides or top).
    #[default]
    Scale,
    /// Exactly the area, aspect ratio ignored.
    Stretch,
    /// The whole area, aspect ratio kept: the overflow is cut off.
    Fill,
    /// One remote pixel per window pixel. Smaller than the area: centred;
    /// larger: the part at the [`View`]'s pan offset shows.
    Original,
}

impl DisplayMode {
    pub const ALL: [DisplayMode; 4] = [
        DisplayMode::Scale,
        DisplayMode::Stretch,
        DisplayMode::Fill,
        DisplayMode::Original,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DisplayMode::Scale => "Scale",
            DisplayMode::Stretch => "Stretch",
            DisplayMode::Fill => "Fill",
            DisplayMode::Original => "Original",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            DisplayMode::Scale => "Scale: fit, keep shape",
            DisplayMode::Stretch => "Stretch: fill, distort",
            DisplayMode::Fill => "Fill: fill, crop edges",
            DisplayMode::Original => "Original size: 1:1 pixels",
        }
    }
}

impl std::str::FromStr for DisplayMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DisplayMode::ALL
            .into_iter()
            .find(|m| m.label().eq_ignore_ascii_case(s))
            .ok_or_else(|| format!("unknown display mode {s:?} (scale, stretch, fill or original)"))
    }
}

/// How the picture is shown: the mode, and for [`DisplayMode::Original`]
/// which part of a picture larger than its area is in view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct View {
    pub mode: DisplayMode,
    /// Picture pixel at the area's top-left corner, where the picture is
    /// larger than the area (clamped when placing, so it may be anything).
    pub pan: (u32, u32),
}

impl From<DisplayMode> for View {
    fn from(mode: DisplayMode) -> Self {
        Self { mode, pan: (0, 0) }
    }
}

/// The largest useful pan offset for a `src_w x src_h` picture in `area`.
pub fn max_pan(src_w: u32, src_h: u32, area: Rect) -> (u32, u32) {
    (src_w.saturating_sub(area.w), src_h.saturating_sub(area.h))
}

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

/// Where the picture is drawn, in window pixels. With [`DisplayMode::Fill`]
/// and [`DisplayMode::Original`] it can overhang the area (so `x`/`y` may be
/// before it); drawing clips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i64,
    pub y: i64,
    pub w: u32,
    pub h: u32,
}

/// Where a `src_w x src_h` picture goes in `area` for `view`.
pub fn place(view: impl Into<View>, src_w: u32, src_h: u32, area: Rect) -> Placement {
    let view = view.into();
    let empty = Placement {
        x: i64::from(area.x),
        y: i64::from(area.y),
        w: 0,
        h: 0,
    };
    if src_w == 0 || src_h == 0 || area.w == 0 || area.h == 0 {
        return empty;
    }
    match view.mode {
        DisplayMode::Scale => {
            let (x, y, w, h) = fit(src_w, src_h, area.w, area.h);
            Placement {
                x: i64::from(area.x + x),
                y: i64::from(area.y + y),
                w,
                h,
            }
        }
        DisplayMode::Stretch => Placement {
            w: area.w,
            h: area.h,
            ..empty
        },
        DisplayMode::Fill => {
            let (sw, sh, dw, dh) = (
                u64::from(src_w),
                u64::from(src_h),
                u64::from(area.w),
                u64::from(area.h),
            );
            // Scale by the larger ratio, so both sides cover the area.
            let (w, h) = if sw * dh > sh * dw {
                (sw * dh / sh, dh)
            } else {
                (dw, sh * dw / sw)
            };
            let (w, h) = (w.max(dw) as u32, h.max(dh) as u32);
            Placement {
                x: i64::from(area.x) - i64::from((w - area.w) / 2),
                y: i64::from(area.y) - i64::from((h - area.h) / 2),
                w,
                h,
            }
        }
        DisplayMode::Original => {
            // Per axis: centred if it fits, else shifted by the pan.
            let axis = |start: u32, room: u32, size: u32, pan: u32| {
                if size <= room {
                    i64::from(start + (room - size) / 2)
                } else {
                    i64::from(start) - i64::from(pan.min(size - room))
                }
            };
            Placement {
                x: axis(area.x, area.w, src_w, view.pan.0),
                y: axis(area.y, area.h, src_h, view.pan.1),
                w: src_w,
                h: src_h,
            }
        }
    }
}

/// Draw `src` into `area` of the `dst_w`-wide buffer `dst` (nearest
/// neighbour), on `BACKGROUND` where it does not cover the area.
pub fn draw_picture(
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst: &mut [u32],
    dst_w: u32,
    area: Rect,
    view: impl Into<View>,
) {
    let p = place(view, src_w, src_h, area);
    // Which source column each column of the area shows (the same for
    // every row), worked out once rather than per pixel.
    let columns: Vec<Option<usize>> = (0..area.w)
        .map(|i| {
            let rx = i64::from(area.x) + i64::from(i) - p.x;
            (p.w > 0 && rx >= 0 && rx < i64::from(p.w))
                .then(|| (rx as u64 * u64::from(src_w) / u64::from(p.w)) as usize)
        })
        .collect();
    for y in area.y..area.bottom() {
        let row = &mut dst[(y * dst_w + area.x) as usize..][..area.w as usize];
        let ry = i64::from(y) - p.y;
        if p.w == 0 || ry < 0 || ry >= i64::from(p.h) {
            row.fill(BACKGROUND);
            continue;
        }
        let sy = (ry as u64 * u64::from(src_h) / u64::from(p.h)) as usize;
        let src_row = &src[sy * src_w as usize..][..src_w as usize];
        for (d, column) in row.iter_mut().zip(&columns) {
            *d = column.map_or(BACKGROUND, |sx| src_row[sx]);
        }
    }
}

/// Inverse of [`draw_picture`]: the picture pixel under window position
/// `(x, y)` (physical pixels), or `None` outside the area or the picture.
pub fn unplace(
    (x, y): (f64, f64),
    src_w: u32,
    src_h: u32,
    area: Rect,
    view: impl Into<View>,
) -> Option<(u32, u32)> {
    if !area.contains((x, y)) {
        return None;
    }
    let p = place(view, src_w, src_h, area);
    let (rx, ry) = (x.floor() - p.x as f64, y.floor() - p.y as f64);
    if p.w == 0 || rx < 0.0 || ry < 0.0 || rx >= f64::from(p.w) || ry >= f64::from(p.h) {
        return None;
    }
    // Same nearest-neighbour mapping as `draw_picture`.
    let px = (rx as u64 * u64::from(src_w) / u64::from(p.w)) as u32;
    let py = (ry as u64 * u64::from(src_h) / u64::from(p.h)) as u32;
    Some((px.min(src_w - 1), py.min(src_h - 1)))
}

/// Where the edge of `area` pans a picture larger than it: the pan speed,
/// in pixels a second per axis (negative: towards the top-left), for the
/// pointer at `pos`. Within `zone` pixels of an edge the speed ramps up to
/// `top_speed` at the edge itself; zero where there is nothing more to see.
pub fn edge_pan(
    pos: (f64, f64),
    area: Rect,
    src: (u32, u32),
    pan: (u32, u32),
    zone: f64,
    top_speed: f64,
) -> (f64, f64) {
    if !area.contains(pos) || zone <= 0.0 {
        return (0.0, 0.0);
    }
    let (max_x, max_y) = max_pan(src.0, src.1, area);
    let axis = |at: f64, start: u32, len: u32, pan: u32, max: u32| {
        let (near, far) = (at - f64::from(start), f64::from(start + len) - at);
        let zone = zone.min(f64::from(len) / 4.0);
        if near < zone && pan.min(max) > 0 {
            -top_speed * (1.0 - near / zone)
        } else if far <= zone && pan < max {
            top_speed * (1.0 - far / zone)
        } else {
            0.0
        }
    };
    (
        axis(pos.0, area.x, area.w, pan.0, max_x),
        axis(pos.1, area.y, area.h, pan.1, max_y),
    )
}

/// For a picture larger than `area` (original size): thin bars along the
/// bottom and right edges showing which part of it is in view.
pub fn draw_pan_bars(
    canvas: &mut Canvas,
    area: Rect,
    src: (u32, u32),
    pan: (u32, u32),
    thickness: u32,
    color: u32,
) {
    let (max_x, max_y) = max_pan(src.0, src.1, area);
    // Visible length and offset along an axis of `len` pixels.
    let thumb = |len: u32, size: u32, pan: u32, max: u32| {
        let w = (u64::from(len) * u64::from(len) / u64::from(size)).max(8) as u32;
        let w = w.min(len);
        let at = (u64::from(len - w) * u64::from(pan.min(max)) / u64::from(max.max(1))) as u32;
        (at, w)
    };
    if max_x > 0 && area.h > thickness {
        let (at, w) = thumb(area.w, src.0, pan.0, max_x);
        let y = area.bottom() - thickness;
        fill_rect(canvas, Rect::new(area.x + at, y, w, thickness), color);
    }
    if max_y > 0 && area.w > thickness {
        let (at, h) = thumb(area.h, src.1, pan.1, max_y);
        let x = area.right() - thickness;
        fill_rect(canvas, Rect::new(x, area.y + at, thickness, h), color);
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

    /// Mix `color` over what is there, `alpha` (0-1) of the way.
    fn blend(&mut self, x: u32, y: u32, color: u32, alpha: f32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let pixel = &mut self.pixels[(y * self.width + x) as usize];
        let alpha = alpha.clamp(0.0, 1.0);
        let mix = |shift: u32| {
            let (under, over) = ((*pixel >> shift) & 0xff, (color >> shift) & 0xff);
            let v = under as f32 + (over as f32 - under as f32) * alpha;
            (v.round() as u32) << shift
        };
        *pixel = mix(16) | mix(8) | mix(0);
    }
}

/// Draw `text` with its top-left at `(x, y)`, each glyph `size` pixels
/// square (and `size` apart).
pub fn draw_text(canvas: &mut Canvas, (x, y): (u32, u32), size: u32, color: u32, text: &str) {
    if size == 0 {
        return;
    }
    // How much of each font pixel (0-7) each output pixel (0..size) covers.
    let weights = axis_weights(size);
    for (i, ch) in text.chars().enumerate() {
        let glyph = font8x8::BASIC_FONTS
            .get(ch)
            .or_else(|| font8x8::BASIC_FONTS.get('?'));
        let Some(glyph) = glyph else { continue };
        let gx = x + i as u32 * size;
        if size.is_multiple_of(8) {
            let scale = size / 8;
            for (row, bits) in glyph.iter().enumerate() {
                for col in (0..8).filter(|col| bits & (1 << col) != 0) {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            canvas.put(gx + col * scale + dx, y + row as u32 * scale + dy, color);
                        }
                    }
                }
            }
            continue;
        }
        for (oy, wy) in weights.iter().enumerate() {
            for (ox, wx) in weights.iter().enumerate() {
                let mut coverage = 0.0;
                for &(row, fy) in wy {
                    let bits = glyph[row];
                    for &(col, fx) in wx {
                        if bits & (1 << col) != 0 {
                            coverage += fy * fx;
                        }
                    }
                }
                if coverage > 0.0 {
                    canvas.blend(gx + ox as u32, y + oy as u32, color, coverage);
                }
            }
        }
    }
}

/// For each of `size` output pixels along an axis: the font pixels (0-7)
/// it overlaps and by what fraction of the output pixel.
fn axis_weights(size: u32) -> Vec<Vec<(usize, f32)>> {
    let step = 8.0 / size as f32;
    (0..size)
        .map(|o| {
            let (from, to) = (o as f32 * step, (o + 1) as f32 * step);
            (from.floor() as usize..(to.ceil() as usize).min(8))
                .filter_map(|i| {
                    let overlap = to.min(i as f32 + 1.0) - from.max(i as f32);
                    (overlap > 0.0).then_some((i, overlap / step))
                })
                .collect()
        })
        .collect()
}

/// Fill `rect`, clipped to the canvas.
pub fn fill_rect(canvas: &mut Canvas, rect: Rect, color: u32) {
    for py in rect.y..rect.bottom().min(canvas.height) {
        let row = (py * canvas.width) as usize;
        let (from, to) = (rect.x.min(canvas.width), rect.right().min(canvas.width));
        canvas.pixels[row + from as usize..row + to as usize].fill(color);
    }
}

/// A one-pixel-per-`t` outline of `rect`.
pub fn outline(canvas: &mut Canvas, rect: Rect, t: u32, color: u32) {
    let t = t.min(rect.w / 2).min(rect.h / 2).max(1);
    fill_rect(canvas, Rect::new(rect.x, rect.y, rect.w, t), color);
    fill_rect(
        canvas,
        Rect::new(rect.x, rect.bottom() - t, rect.w, t),
        color,
    );
    fill_rect(canvas, Rect::new(rect.x, rect.y, t, rect.h), color);
    fill_rect(
        canvas,
        Rect::new(rect.right() - t, rect.y, t, rect.h),
        color,
    );
}

/// A small downward triangle (a drop-down marker) `size` wide at `(x, y)`.
pub fn draw_caret(canvas: &mut Canvas, (x, y): (u32, u32), size: u32, color: u32) {
    for row in 0..size / 2 {
        fill_rect(
            canvas,
            Rect::new(x + row, y + row, size - 2 * row, 1),
            color,
        );
    }
}

/// Pixel width of `text` with glyphs `size` pixels wide.
pub fn text_width(text: &str, size: u32) -> u32 {
    text.chars().count() as u32 * size
}

/// A one-line status message centred in `area` (connecting, closed...),
/// glyphs `size` pixels.
pub fn draw_status(canvas: &mut Canvas, area: Rect, size: u32, text: &str) {
    let w = text_width(text, size);
    let x = area.x + area.w.saturating_sub(w) / 2;
    let y = area.y + area.h.saturating_sub(size) / 2;
    draw_text(canvas, (x, y), size, 0x00e8_e8e8, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = Rect::new(0, 0, 1000, 1000);

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
    fn modes_place_the_picture() {
        let area = Rect::new(100, 50, 1000, 1000);
        let at = |x, y, w, h| Placement { x, y, w, h };
        // 16:9 in a square: bars top and bottom...
        assert_eq!(
            place(DisplayMode::Scale, 1920, 1080, area),
            at(100, 269, 1000, 562)
        );
        // ...squashed into the square...
        assert_eq!(
            place(DisplayMode::Stretch, 1920, 1080, area),
            at(100, 50, 1000, 1000)
        );
        // ...or full height with the sides cut off, centred.
        assert_eq!(
            place(DisplayMode::Fill, 1920, 1080, area),
            at(-288, 50, 1777, 1000)
        );
        // Tall picture in a wide area: Fill cuts top and bottom.
        let wide = Rect::new(0, 0, 400, 100);
        assert_eq!(
            place(DisplayMode::Fill, 100, 200, wide),
            at(0, -350, 400, 800)
        );
        assert_eq!(place(DisplayMode::Fill, 0, 0, wide).w, 0);
    }

    #[test]
    fn original_size_centres_a_small_picture_and_pans_a_large_one() {
        let area = Rect::new(100, 50, 1000, 600);
        let at = |x, y, w, h| Placement { x, y, w, h };
        assert_eq!(
            place(DisplayMode::Original, 800, 400, area),
            at(200, 150, 800, 400)
        );
        let big = |pan| View {
            mode: DisplayMode::Original,
            pan,
        };
        assert_eq!(
            place(big((0, 0)), 1920, 1080, area),
            at(100, 50, 1920, 1080)
        );
        assert_eq!(
            place(big((300, 200)), 1920, 1080, area),
            at(-200, -150, 1920, 1080)
        );
        // Panning stops at the far edge.
        assert_eq!(
            place(big((5000, 5000)), 1920, 1080, area),
            at(-820, -430, 1920, 1080)
        );
        // Wider than the area but not taller: pans across, centred down.
        assert_eq!(
            place(big((50, 99)), 1200, 400, area),
            at(50, 150, 1200, 400)
        );
        assert_eq!(max_pan(1920, 1080, area), (920, 480));
        assert_eq!(max_pan(800, 400, area), (0, 0));

        // One window pixel per picture pixel, pan included.
        assert_eq!(
            unplace((100.0, 50.0), 1920, 1080, area, big((300, 200))),
            Some((300, 200))
        );
        assert_eq!(
            unplace((1099.5, 649.5), 1920, 1080, area, big((300, 200))),
            Some((1299, 799))
        );
    }

    #[test]
    fn the_edges_pan_only_towards_what_is_out_of_view() {
        let area = Rect::new(0, 0, 1000, 600);
        let src = (1920, 1080);
        let pan = |pos, at| edge_pan(pos, area, src, at, 50.0, 1000.0);
        assert_eq!(pan((500.0, 300.0), (0, 0)), (0.0, 0.0), "middle");
        // Right edge: onwards; at the very edge, full speed.
        assert_eq!(pan((999.0, 300.0), (0, 0)), (980.0, 0.0));
        // Left edge at the start: nothing more to the left.
        assert_eq!(pan((0.0, 300.0), (0, 0)), (0.0, 0.0));
        assert_eq!(pan((0.0, 300.0), (10, 0)), (-1000.0, 0.0));
        // Bottom-right corner, already at the end across.
        assert_eq!(pan((999.0, 599.0), (920, 0)).0, 0.0);
        assert!(pan((999.0, 599.0), (920, 0)).1 > 0.0);
        // A picture that fits never pans; outside the area neither.
        assert_eq!(
            edge_pan((999.0, 300.0), area, (800, 400), (0, 0), 50.0, 1000.0),
            (0.0, 0.0)
        );
        assert_eq!(pan((1000.0, 300.0), (0, 0)), (0.0, 0.0));
    }

    #[test]
    fn unplace_finds_the_picture_pixel_under_the_cursor() {
        assert_eq!(
            unplace((0.0, 219.0), 1920, 1080, WINDOW, DisplayMode::Scale),
            Some((0, 0))
        );
        assert_eq!(
            unplace((999.9, 780.9), 1920, 1080, WINDOW, DisplayMode::Scale),
            Some((1918, 1078))
        );
        assert_eq!(
            unplace((500.0, 100.0), 1920, 1080, WINDOW, DisplayMode::Scale),
            None,
            "bar"
        );
        assert_eq!(
            unplace((-1.0, 500.0), 1920, 1080, WINDOW, DisplayMode::Scale),
            None
        );
        // Stretch covers the area; Fill's left edge is cut off.
        assert_eq!(
            unplace((500.0, 0.0), 1920, 1080, WINDOW, DisplayMode::Stretch),
            Some((960, 0))
        );
        assert_eq!(
            unplace((0.0, 0.0), 1920, 1080, WINDOW, DisplayMode::Fill),
            Some((419, 0))
        );
        // Never outside the area, even where a Fill picture overhangs it.
        let area = Rect::new(100, 0, 100, 100);
        assert_eq!(
            unplace((50.0, 50.0), 400, 100, area, DisplayMode::Fill),
            None
        );
    }

    #[test]
    fn unplace_agrees_with_draw_picture_in_every_mode() {
        // Each pixel's colour encodes its position; what is drawn at a window
        // position must be the pixel unplace reports for it.
        let (sw, sh, dw, dh) = (37u32, 23u32, 101u32, 77u32);
        let src: Vec<u32> = (1..=sw * sh).collect();
        let area = Rect::new(7, 5, 80, 60);
        for mode in DisplayMode::ALL {
            let mut dst = vec![0u32; (dw * dh) as usize];
            let view = View { mode, pan: (5, 3) };
            draw_picture(&src, sw, sh, &mut dst, dw, area, view);
            for y in 0..dh {
                for x in 0..dw {
                    let drawn = dst[(y * dw + x) as usize];
                    let pos = (f64::from(x) + 0.5, f64::from(y) + 0.5);
                    match unplace(pos, sw, sh, area, view) {
                        Some((px, py)) => assert_eq!(drawn, py * sw + px + 1, "{mode:?} {x},{y}"),
                        None if area.contains(pos) => {
                            assert_eq!(drawn, BACKGROUND, "{mode:?} {x},{y}")
                        }
                        // Outside the area nothing is drawn.
                        None => assert_eq!(drawn, 0, "{mode:?} {x},{y}"),
                    }
                }
            }
        }
    }

    #[test]
    fn scaling_samples_the_right_source_pixels() {
        // 2x2 source: four distinct colours, drawn into 4x4.
        let src = [1, 2, 3, 4];
        let mut dst = vec![0; 16];
        draw_picture(
            &src,
            2,
            2,
            &mut dst,
            4,
            Rect::new(0, 0, 4, 4),
            DisplayMode::Scale,
        );
        assert_eq!(dst, [1, 1, 2, 2, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 4, 4]);
    }

    #[test]
    fn letterbox_area_is_background() {
        let src = [7; 4];
        let mut dst = vec![0; 4 * 2];
        draw_picture(
            &src,
            2,
            2,
            &mut dst,
            4,
            Rect::new(0, 0, 4, 2),
            DisplayMode::Scale,
        );
        assert_eq!(
            dst,
            [BACKGROUND, 7, 7, BACKGROUND, BACKGROUND, 7, 7, BACKGROUND]
        );
        // Stretched, the same picture covers it.
        draw_picture(
            &src,
            2,
            2,
            &mut dst,
            4,
            Rect::new(0, 0, 4, 2),
            DisplayMode::Stretch,
        );
        assert_eq!(dst, [7; 8]);
    }

    #[test]
    fn display_modes_parse() {
        assert_eq!("fill".parse::<DisplayMode>(), Ok(DisplayMode::Fill));
        assert_eq!("Stretch".parse::<DisplayMode>(), Ok(DisplayMode::Stretch));
        assert!("zoom".parse::<DisplayMode>().is_err());
    }

    #[test]
    fn text_is_drawn_and_clipped() {
        let mut dst = vec![0; 16 * 8];
        let mut canvas = Canvas {
            pixels: &mut dst,
            width: 16,
            height: 8,
        };
        draw_text(&mut canvas, (0, 0), 8, 0xff, "II");
        // Off-screen text and boxes must not panic.
        draw_text(&mut canvas, (12, 4), 24, 0xff, "clipped");
        draw_text(&mut canvas, (9, 1), 12, 0xff, "odd sizes clip too");
        fill_rect(&mut canvas, Rect::new(10, 5, 100, 100), 0xee);
        outline(&mut canvas, Rect::new(0, 0, 16, 8), 1, 0xdd);
        draw_caret(&mut canvas, (12, 0), 8, 0xcc);
        assert!(dst.contains(&0xff) && dst.contains(&0xee) && dst[0] == 0xdd);
        assert_eq!(text_width("abc", 16), 48);
    }

    #[test]
    fn scaled_text_keeps_the_glyph_and_anti_aliases_it() {
        let render = |size: u32| {
            let mut dst = vec![0u32; (size * size) as usize];
            let mut canvas = Canvas {
                pixels: &mut dst,
                width: size,
                height: size,
            };
            draw_text(&mut canvas, (0, 0), size, 0x00ff_ffff, "H");
            dst
        };
        // 16 px: every pixel fully on or off, as before.
        assert!(render(16).iter().all(|&p| p == 0 || p == 0x00ff_ffff));
        // 12 px: the same total ink (area-averaged), with grey edges.
        let ink = |pixels: &[u32], size: u32| {
            pixels
                .iter()
                .map(|p| f64::from(p & 0xff) / 255.0)
                .sum::<f64>()
                / f64::from(size * size)
        };
        let (big, small) = (render(16), render(12));
        assert!((ink(&big, 16) - ink(&small, 12)).abs() < 0.02);
        assert!(
            small.iter().any(|&p| p != 0 && p != 0x00ff_ffff),
            "anti-aliased"
        );
        // Weights along an axis cover each output pixel exactly once.
        for size in [5, 10, 12, 13, 20] {
            for w in axis_weights(size) {
                let total: f32 = w.iter().map(|(_, f)| f).sum();
                assert!((total - 1.0).abs() < 1e-4, "{size}: {w:?}");
            }
        }
    }
}
