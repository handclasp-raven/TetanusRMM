//! Drawing into the window's pixel buffer: the remote picture placed in
//! its area (scaled to fit, stretched, or filling it), and what the
//! viewer's own controls are made of, so the viewer needs no GUI toolkit:
//! text in the built-in typeface, rounded boxes, stroked icons and the
//! mark, all anti-aliased.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use brand::icons::Icon;
use brand::path::{Point, Seg};
use brand::raster::Image;
use brand::Rgb;

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
/// neighbour), on `background` where it does not cover the area.
#[allow(clippy::too_many_arguments)]
pub fn draw_picture(
    src: &[u32],
    src_w: u32,
    src_h: u32,
    dst: &mut [u32],
    dst_w: u32,
    area: Rect,
    view: impl Into<View>,
    background: u32,
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
            row.fill(background);
            continue;
        }
        let sy = (ry as u64 * u64::from(src_h) / u64::from(p.h)) as usize;
        let src_row = &src[sy * src_w as usize..][..src_w as usize];
        for (d, column) in row.iter_mut().zip(&columns) {
            *d = column.map_or(background, |sx| src_row[sx]);
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
    /// Mix `color` over what is there, `alpha` (0-1) of the way. Off the
    /// canvas, nothing.
    fn blend(&mut self, x: i64, y: i64, color: u32, alpha: f32) {
        if x < 0 || y < 0 || x >= i64::from(self.width) || y >= i64::from(self.height) {
            return;
        }
        let pixel = &mut self.pixels[(y * i64::from(self.width) + x) as usize];
        if alpha >= 1.0 {
            *pixel = color;
            return;
        }
        let alpha = alpha.max(0.0);
        let mix = |shift: u32| {
            let (under, over) = ((*pixel >> shift) & 0xff, (color >> shift) & 0xff);
            let v = under as f32 + (over as f32 - under as f32) * alpha;
            (v.round() as u32) << shift
        };
        *pixel = mix(16) | mix(8) | mix(0);
    }
}

/// The two weights of the typeface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weight {
    Regular,
    SemiBold,
}

/// How a piece of text is set: its size (the em, in pixels), its weight,
/// and extra room after each character (headings are spaced out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub px: u32,
    pub weight: Weight,
    pub tracking: u32,
}

impl Style {
    pub const fn new(px: u32) -> Self {
        Self {
            px,
            weight: Weight::Regular,
            tracking: 0,
        }
    }

    pub const fn semibold(self) -> Self {
        Self {
            weight: Weight::SemiBold,
            ..self
        }
    }

    pub const fn tracked(self, tracking: u32) -> Self {
        Self { tracking, ..self }
    }

    /// How far apart characters are. The typeface is monospaced, 0.6 em a
    /// character; they are put on whole pixels so text stays sharp.
    pub fn advance(self) -> u32 {
        char_w(self.px) + self.tracking
    }
}

/// Width of a character of text `px` high.
pub fn char_w(px: u32) -> u32 {
    ((px * 3 + 2) / 5).max(1)
}

/// How far below the top of its line text's baseline is, as a share of
/// its size: capitals end up centred in the line.
const BASELINE: f32 = 0.86;

/// Coverage (0-255) of a shape, and where its top-left corner goes
/// relative to where it is drawn.
struct Mask {
    w: u32,
    dx: i32,
    dy: i32,
    alpha: Vec<u8>,
}

impl Mask {
    fn draw(&self, canvas: &mut Canvas, (x, y): (u32, u32), color: u32) {
        let (x, y) = (
            i64::from(x) + i64::from(self.dx),
            i64::from(y) + i64::from(self.dy),
        );
        for (i, &a) in self.alpha.iter().enumerate().filter(|(_, a)| **a > 0) {
            let (col, row) = (i as u32 % self.w, i as u32 / self.w);
            canvas.blend(
                x + i64::from(col),
                y + i64::from(row),
                color,
                f32::from(a) / 255.0,
            );
        }
    }
}

/// The typeface (IBM Plex Mono, built in), the icons and the mark, as
/// pixels: each is worked out once per size and kept.
pub struct Glyphs {
    regular: fontdue::Font,
    semibold: fontdue::Font,
    text: RefCell<HashMap<(Weight, u32, char), Rc<Mask>>>,
    icons: RefCell<HashMap<(&'static str, u32), Rc<Mask>>>,
    marks: RefCell<HashMap<(u32, Rgb), Rc<Image>>>,
}

impl Default for Glyphs {
    fn default() -> Self {
        Self::new()
    }
}

impl Glyphs {
    pub fn new() -> Self {
        let font = |bytes: &[u8]| {
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                .expect("the built-in typeface is a valid font")
        };
        Self {
            regular: font(include_bytes!("../assets/IBMPlexMono-Regular.ttf")),
            semibold: font(include_bytes!("../assets/IBMPlexMono-SemiBold.ttf")),
            text: RefCell::default(),
            icons: RefCell::default(),
            marks: RefCell::default(),
        }
    }

    fn glyph(&self, style: Style, ch: char) -> Rc<Mask> {
        self.text
            .borrow_mut()
            .entry((style.weight, style.px, ch))
            .or_insert_with(|| {
                let font = match style.weight {
                    Weight::Regular => &self.regular,
                    Weight::SemiBold => &self.semibold,
                };
                let (m, alpha) = font.rasterize(ch, style.px as f32);
                let baseline = (style.px as f32 * BASELINE).round() as i32;
                Rc::new(Mask {
                    w: m.width as u32,
                    dx: m.xmin,
                    dy: baseline - m.height as i32 - m.ymin,
                    alpha,
                })
            })
            .clone()
    }

    fn icon(&self, icon: &Icon, size: u32) -> Rc<Mask> {
        self.icons
            .borrow_mut()
            .entry((icon.name, size))
            .or_insert_with(|| Rc::new(icon_mask(icon, size)))
            .clone()
    }

    fn mark(&self, size: u32, tile: Rgb) -> Rc<Image> {
        self.marks
            .borrow_mut()
            .entry((size, tile))
            .or_insert_with(|| Rc::new(brand::raster::app_icon(size, tile)))
            .clone()
    }
}

/// An icon's strokes as straight pieces, each with half its width, in the
/// icon's own units.
fn icon_lines(icon: &Icon) -> Vec<(Point, Point, f32)> {
    /// Straight pieces a curve is cut into.
    const STEPS: u32 = 12;
    let mut lines = Vec::new();
    for (width, segs) in icon.outlines() {
        let (mut at, mut start) = ((0.0, 0.0), (0.0, 0.0));
        let mut line = |from: Point, to: Point| lines.push((from, to, width / 2.0));
        for seg in segs {
            match seg {
                Seg::Move(p) => (at, start) = (p, p),
                Seg::Line(p) => {
                    line(at, p);
                    at = p;
                }
                Seg::Cubic(c1, c2, p) => {
                    let from = at;
                    for step in 1..=STEPS {
                        let t = step as f32 / STEPS as f32;
                        let u = 1.0 - t;
                        let on = |a: f32, b: f32, c: f32, d: f32| {
                            u * u * u * a
                                + 3.0 * u * u * t * b
                                + 3.0 * u * t * t * c
                                + t * t * t * d
                        };
                        let to = (on(from.0, c1.0, c2.0, p.0), on(from.1, c1.1, c2.1, p.1));
                        line(at, to);
                        at = to;
                    }
                }
                Seg::Close => {
                    line(at, start);
                    at = start;
                }
            }
        }
    }
    lines
}

/// `icon` drawn `size` pixels square: each pixel is covered by how far
/// inside the nearest stroke its centre is (round caps and joins fall
/// out of that).
fn icon_mask(icon: &Icon, size: u32) -> Mask {
    let lines = icon_lines(icon);
    let scale = size as f32 / brand::icons::SIZE;
    let mut alpha = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let p = ((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale);
            let cover = lines
                .iter()
                .map(|&(a, b, half)| (half - distance(p, a, b)) * scale + 0.5)
                .fold(0.0f32, f32::max);
            alpha.push((cover.min(1.0) * 255.0).round() as u8);
        }
    }
    Mask {
        w: size,
        dx: 0,
        dy: 0,
        alpha,
    }
}

/// How far `p` is from the piece of line from `a` to `b`.
fn distance(p: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len > 0.0 {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p.0 - (a.0 + t * dx)).hypot(p.1 - (a.1 + t * dy))
}

/// Draw `text` with the top-left of its first character's cell at
/// `(x, y)`: the cell is `style.px` high and [`Style::advance`] wide.
pub fn draw_text(
    canvas: &mut Canvas,
    glyphs: &Glyphs,
    (x, y): (u32, u32),
    style: Style,
    color: u32,
    text: &str,
) {
    if style.px == 0 || y >= canvas.height {
        return;
    }
    for (i, ch) in text.chars().enumerate() {
        let gx = x.saturating_add(i as u32 * style.advance());
        if gx >= canvas.width {
            break;
        }
        if ch != ' ' {
            glyphs.glyph(style, ch).draw(canvas, (gx, y), color);
        }
    }
}

/// Pixel width of `text` set in `style`.
pub fn text_width(text: &str, style: Style) -> u32 {
    text.chars().count() as u32 * style.advance()
}

/// Draw `icon` `size` pixels square with its top-left at `pos`.
pub fn draw_icon(
    canvas: &mut Canvas,
    glyphs: &Glyphs,
    pos: (u32, u32),
    size: u32,
    color: u32,
    icon: &Icon,
) {
    if size > 0 {
        glyphs.icon(icon, size).draw(canvas, pos, color);
    }
}

/// Draw the mark (white, on a tile of `tile`) `size` pixels square.
pub fn draw_mark(canvas: &mut Canvas, glyphs: &Glyphs, (x, y): (u32, u32), size: u32, tile: Rgb) {
    if size == 0 {
        return;
    }
    let image = glyphs.mark(size, tile);
    for py in 0..size {
        for px in 0..size {
            let [r, g, b, a] = image.pixel(px, py);
            if a > 0 {
                canvas.blend(
                    i64::from(x) + i64::from(px),
                    i64::from(y) + i64::from(py),
                    crate::theme::pixel(Rgb(r, g, b)),
                    f32::from(a) / 255.0,
                );
            }
        }
    }
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

/// The four corners of `rect`, each `r` pixels square: `cover` says how
/// much of a pixel is drawn, from how far its centre is from the centre
/// of its corner's circle.
fn corners(canvas: &mut Canvas, rect: Rect, r: u32, color: u32, cover: impl Fn(f32) -> f32) {
    let (left, top) = (i64::from(rect.x), i64::from(rect.y));
    let (right, bottom) = (i64::from(rect.right()) - 1, i64::from(rect.bottom()) - 1);
    for j in 0..r {
        for i in 0..r {
            let d = ((r - i) as f32 - 0.5).hypot((r - j) as f32 - 0.5);
            let alpha = cover(d).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            let (i, j) = (i64::from(i), i64::from(j));
            canvas.blend(left + i, top + j, color, alpha);
            canvas.blend(right - i, top + j, color, alpha);
            canvas.blend(left + i, bottom - j, color, alpha);
            canvas.blend(right - i, bottom - j, color, alpha);
        }
    }
}

/// Fill `rect` with its corners rounded to radius `r`.
pub fn fill_round_rect(canvas: &mut Canvas, rect: Rect, r: u32, color: u32) {
    let r = r.min(rect.w / 2).min(rect.h / 2);
    if r == 0 {
        return fill_rect(canvas, rect, color);
    }
    fill_rect(
        canvas,
        Rect::new(rect.x, rect.y + r, rect.w, rect.h - 2 * r),
        color,
    );
    let across = Rect::new(rect.x + r, rect.y, rect.w - 2 * r, r);
    fill_rect(canvas, across, color);
    fill_rect(
        canvas,
        Rect {
            y: rect.bottom() - r,
            ..across
        },
        color,
    );
    let radius = r as f32;
    corners(canvas, rect, r, color, |d| radius - d + 0.5);
}

/// An outline of `rect`, `t` thick, with its corners rounded to radius `r`.
pub fn outline_round_rect(canvas: &mut Canvas, rect: Rect, r: u32, t: u32, color: u32) {
    let r = r.min(rect.w / 2).min(rect.h / 2);
    if r == 0 {
        return outline(canvas, rect, t, color);
    }
    let t = t.min(r).max(1);
    let across = Rect::new(rect.x + r, rect.y, rect.w - 2 * r, t);
    fill_rect(canvas, across, color);
    fill_rect(
        canvas,
        Rect {
            y: rect.bottom() - t,
            ..across
        },
        color,
    );
    let down = Rect::new(rect.x, rect.y + r, t, rect.h - 2 * r);
    fill_rect(canvas, down, color);
    fill_rect(
        canvas,
        Rect {
            x: rect.right() - t,
            ..down
        },
        color,
    );
    let (outer, inner) = (r as f32, (r - t) as f32);
    corners(canvas, rect, r, color, |d| {
        (outer - d + 0.5).min(d - inner + 0.5)
    });
}

/// A dashed outline of `rect`, `t` thick: `dash` pixels drawn, then as
/// many left out.
pub fn dashed_outline(canvas: &mut Canvas, rect: Rect, t: u32, dash: u32, color: u32) {
    let t = t.min(rect.w / 2).min(rect.h / 2).max(1);
    let dash = dash.max(1);
    for x in (rect.x..rect.right()).step_by(2 * dash as usize) {
        let w = dash.min(rect.right() - x);
        fill_rect(canvas, Rect::new(x, rect.y, w, t), color);
        fill_rect(canvas, Rect::new(x, rect.bottom() - t, w, t), color);
    }
    for y in (rect.y..rect.bottom()).step_by(2 * dash as usize) {
        let h = dash.min(rect.bottom() - y);
        fill_rect(canvas, Rect::new(rect.x, y, t, h), color);
        fill_rect(canvas, Rect::new(rect.right() - t, y, t, h), color);
    }
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

/// A one-line status message centred in `area` (connecting, closed...).
pub fn draw_status(
    canvas: &mut Canvas,
    glyphs: &Glyphs,
    area: Rect,
    style: Style,
    color: u32,
    text: &str,
) {
    let w = text_width(text, style);
    let x = area.x + area.w.saturating_sub(w) / 2;
    let y = area.y + area.h.saturating_sub(style.px) / 2;
    draw_text(canvas, glyphs, (x, y), style, color, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BACKGROUND: u32 = 0x0010_1010;

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
            draw_picture(&src, sw, sh, &mut dst, dw, area, view, BACKGROUND);
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
            BACKGROUND,
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
            BACKGROUND,
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
            BACKGROUND,
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
        let glyphs = Glyphs::new();
        let mut dst = vec![0; 16 * 12];
        let mut canvas = Canvas {
            pixels: &mut dst,
            width: 16,
            height: 12,
        };
        draw_text(&mut canvas, &glyphs, (0, 0), Style::new(12), 0xff, "II");
        let ink = canvas.pixels.iter().filter(|&&p| p != 0).count();
        assert!(ink > 10, "{ink}");
        // Off-screen text and boxes must not panic.
        draw_text(
            &mut canvas,
            &glyphs,
            (12, 4),
            Style::new(24),
            0xff,
            "clipped",
        );
        draw_text(&mut canvas, &glyphs, (9, 20), Style::new(12), 0xff, "below");
        fill_rect(&mut canvas, Rect::new(10, 5, 100, 100), 0xee);
        outline(&mut canvas, Rect::new(0, 0, 16, 12), 1, 0xdd);
        draw_caret(&mut canvas, (12, 0), 8, 0xcc);
        dashed_outline(&mut canvas, Rect::new(2, 2, 40, 40), 1, 3, 0xbb);
        assert!(dst.contains(&0xee) && dst[0] == 0xdd);
    }

    #[test]
    fn text_sits_on_a_grid_of_whole_pixels() {
        // 0.6 em a character, as the typeface is cut.
        assert_eq!([10, 12, 13, 16, 20].map(char_w), [6, 7, 8, 10, 12]);
        assert_eq!(text_width("abc", Style::new(12)), 21);
        // Spaced out, each character takes that much more.
        assert_eq!(text_width("abc", Style::new(12).tracked(2)), 27);
        let glyphs = Glyphs::new();
        let ink = |style: Style, text: &str| {
            let mut dst = vec![0u32; 64 * 24];
            let mut canvas = Canvas {
                pixels: &mut dst,
                width: 64,
                height: 24,
            };
            draw_text(&mut canvas, &glyphs, (2, 2), style, 0x00ff_ffff, text);
            dst
        };
        // Anti-aliased, inside its line, and heavier when semibold.
        let regular = ink(Style::new(16), "Hg");
        assert!(regular.iter().any(|&p| p != 0 && p != 0x00ff_ffff));
        let total = |pixels: &[u32]| pixels.iter().map(|p| u64::from(p & 0xff)).sum::<u64>();
        assert!(total(&ink(Style::new(16).semibold(), "Hg")) > total(&regular));
        // The capital stands on the baseline, inside the 16 px line; only
        // the g's tail goes below it.
        let rows_with_ink = |pixels: &[u32]| -> Vec<usize> {
            (0..24)
                .filter(|y| pixels[y * 64..(y + 1) * 64].iter().any(|&p| p != 0))
                .collect()
        };
        let h = rows_with_ink(&ink(Style::new(16), "H"));
        assert!(h[0] >= 2 && *h.last().unwrap() < 18, "{h:?}");
        assert!(rows_with_ink(&regular).last() > h.last());
        // A space draws nothing; drawing again reuses what was worked out.
        assert!(ink(Style::new(16), "  ").iter().all(|&p| p == 0));
        assert!(regular == ink(Style::new(16), "Hg"));
    }

    #[test]
    fn rounded_boxes_cut_their_corners() {
        let mut dst = vec![0u32; 40 * 30];
        let mut canvas = Canvas {
            pixels: &mut dst,
            width: 40,
            height: 30,
        };
        let rect = Rect::new(4, 3, 30, 20);
        fill_round_rect(&mut canvas, rect, 6, 0x00ff_ffff);
        let at = |pixels: &[u32], x: u32, y: u32| pixels[(y * 40 + x) as usize];
        // The very corner is left alone, the middle and the edges' middles
        // filled, and nothing outside is touched.
        assert_eq!(at(&dst, 4, 3), 0);
        assert_eq!(at(&dst, 33, 22), 0);
        assert_eq!(at(&dst, 19, 12), 0x00ff_ffff);
        assert_eq!(at(&dst, 19, 3), 0x00ff_ffff);
        assert_eq!(at(&dst, 4, 12), 0x00ff_ffff);
        assert!(
            dst.iter().any(|&p| p != 0 && p != 0x00ff_ffff),
            "anti-aliased"
        );
        for y in 0..30 {
            for x in 0..40 {
                let inside = (4..34).contains(&x) && (3..23).contains(&y);
                assert!(inside || at(&dst, x, y) == 0, "{x},{y}");
            }
        }
        // An outline leaves the inside alone.
        let mut dst = vec![0u32; 40 * 30];
        let mut canvas = Canvas {
            pixels: &mut dst,
            width: 40,
            height: 30,
        };
        outline_round_rect(&mut canvas, rect, 6, 1, 0x00ff_ffff);
        // Radius 0, and boxes smaller than their radius, still draw.
        fill_round_rect(&mut canvas, Rect::new(0, 0, 3, 3), 0, 1);
        fill_round_rect(&mut canvas, Rect::new(0, 0, 3, 30), 8, 1);
        outline_round_rect(&mut canvas, Rect::new(36, 26, 20, 20), 4, 1, 1);
        assert_eq!(at(&dst, 19, 12), 0);
        assert_eq!(at(&dst, 19, 3), 0x00ff_ffff);
        assert_eq!(at(&dst, 33, 12), 0x00ff_ffff);
        assert_eq!(at(&dst, 4, 3), 0);
    }

    #[test]
    fn icons_and_the_mark_are_drawn_inside_their_box() {
        let glyphs = Glyphs::new();
        for icon in crate::icons::ALL {
            for size in [14, 18, 28] {
                let mask = icon_mask(&icon, size);
                let ink: u32 = mask.alpha.iter().map(|&a| u32::from(a)).sum();
                // Strokes, not a blob: some of the box, far from all of it.
                let share = ink as f32 / (255 * size * size) as f32;
                assert!(
                    (0.04..0.6).contains(&share),
                    "{} at {size}: {share}",
                    icon.name
                );
            }
        }
        let mut dst = vec![0u32; 40 * 40];
        let mut canvas = Canvas {
            pixels: &mut dst,
            width: 40,
            height: 40,
        };
        draw_icon(&mut canvas, &glyphs, (30, 30), 18, 0xff, &crate::icons::KEY);
        draw_mark(&mut canvas, &glyphs, (2, 2), 18, Rgb::hex(0xC9542A));
        // The tile's colour in its middle left, the white nail in the middle.
        assert_eq!(dst[11 * 40 + 4], 0x00C9_542A);
        assert_eq!(dst[11 * 40 + 11], 0x00FF_FFFF);
        assert_eq!(dst[2 * 40 + 2], 0, "the tile's corner is rounded");
    }
}
