//! Drawing. A [`Painter`] holds what Direct2D and DirectWrite need (one
//! per UI thread); it draws into a [`Surface`], a bitmap with an alpha
//! channel that a window then shows, either copied to its client area or
//! as the whole of a layered window (which is how the session bar and the
//! tray flyout get their rounded corners and shadows).

use std::cell::{Cell, OnceCell};

use brand::icons::Icon;
use brand::mark::{Mark, Variant, BOX};
use brand::path::Seg;
use brand::Rgb;
use windows::core::{w, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED,
    D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_PIXEL_FORMAT,
    D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Brush, ID2D1DCRenderTarget, ID2D1Factory, ID2D1PathGeometry,
    ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_ROUND,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_LINE_JOIN_ROUND,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_numerics::{Matrix3x2, Vector2};

use super::look::Logo;

/// A rectangle, in device-independent pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Grown by `by` on every side (shrunk, if negative).
    pub fn inflate(&self, by: f32) -> Rect {
        Rect::new(
            self.x - by,
            self.y - by,
            self.w + 2.0 * by,
            self.h + 2.0 * by,
        )
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.x,
            top: self.y,
            right: self.right(),
            bottom: self.bottom(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Weight {
    #[default]
    Regular,
    Semibold,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// How a piece of text is set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Text {
    pub size: f32,
    pub weight: Weight,
    pub color: Rgb,
    pub align: Align,
    /// Centred between the rectangle's top and bottom, not at its top.
    pub middle: bool,
    /// Broken over lines to fit the rectangle's width.
    pub wrap: bool,
}

impl Text {
    pub const fn new(size: f32, color: Rgb) -> Self {
        Self {
            size,
            weight: Weight::Regular,
            color,
            align: Align::Left,
            middle: false,
            wrap: false,
        }
    }

    pub const fn weight(mut self, weight: Weight) -> Self {
        self.weight = weight;
        self
    }

    pub const fn semibold(self) -> Self {
        self.weight(Weight::Semibold)
    }

    pub const fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub const fn centered(mut self) -> Self {
        self.align = Align::Center;
        self.middle = true;
        self
    }

    pub const fn middle(mut self) -> Self {
        self.middle = true;
        self
    }

    pub const fn wrap(mut self) -> Self {
        self.wrap = true;
        self
    }
}

/// A bitmap to draw into: 32 bits a pixel with (premultiplied) alpha, top
/// row first, selected into a memory device context.
pub struct Surface {
    hdc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    pub width: i32,
    pub height: i32,
}

impl Surface {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        let (width, height) = (width.max(1), height.max(1));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative: the first row is the top one.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: a memory DC and a DIB section of our own, released in
        // `drop`; `bits` is only an out-parameter here.
        unsafe {
            let hdc = CreateCompatibleDC(None);
            let mut bits = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(hdc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(bitmap) => bitmap,
                    Err(e) => {
                        let _ = DeleteDC(hdc);
                        return Err(e);
                    }
                };
            let previous = SelectObject(hdc, bitmap.into());
            Ok(Self {
                hdc,
                bitmap,
                previous,
                width,
                height,
            })
        }
    }

    pub fn hdc(&self) -> HDC {
        self.hdc
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: our own objects, in the order GDI wants them released.
        unsafe {
            SelectObject(self.hdc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.hdc);
        }
    }
}

fn color(c: Rgb, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(c.0) / 255.0,
        g: f32::from(c.1) / 255.0,
        b: f32::from(c.2) / 255.0,
        a: alpha,
    }
}

fn point((x, y): (f32, f32)) -> Vector2 {
    Vector2 { X: x, Y: y }
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// What drawing needs, made once per UI thread (see [`with_painter`]).
pub struct Painter {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: ID2D1DCRenderTarget,
    /// Round caps and joins, for the icons.
    round: ID2D1StrokeStyle,
    /// The families for headings and for everything else: Segoe UI
    /// Variable where Windows has it (11), else Segoe UI.
    display: PCWSTR,
    body: PCWSTR,
}

thread_local! {
    static PAINTER: OnceCell<Option<Painter>> = const { OnceCell::new() };
}

/// Draw (or measure) with this thread's painter. `None` if Direct2D could
/// not be set up, which is logged once.
pub fn with_painter<R>(f: impl FnOnce(&Painter) -> R) -> Option<R> {
    PAINTER.with(|cell| {
        cell.get_or_init(|| match Painter::new() {
            Ok(painter) => Some(painter),
            Err(e) => {
                tracing::warn!("Direct2D is unavailable, windows will be blank: {e}");
                None
            }
        })
        .as_ref()
        .map(f)
    })
}

impl Painter {
    fn new() -> Result<Self> {
        // SAFETY: creating COM objects with valid, owned arguments.
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let target = d2d.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            })?;
            let round = d2d.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    miterLimit: 10.0,
                    ..Default::default()
                },
                None,
            )?;
            let has = |family: PCWSTR| {
                let mut collection: Option<IDWriteFontCollection> = None;
                if dwrite
                    .GetSystemFontCollection(&mut collection, false)
                    .is_err()
                {
                    return false;
                }
                let (mut index, mut exists) = (0u32, BOOL(0));
                collection.is_some_and(|c| {
                    c.FindFamilyName(family, &mut index, &mut exists).is_ok() && exists.as_bool()
                })
            };
            let (display, body) = (
                w!("Segoe UI Variable Display"),
                w!("Segoe UI Variable Text"),
            );
            let fallback = w!("Segoe UI");
            Ok(Self {
                display: if has(display) { display } else { fallback },
                body: if has(body) { body } else { fallback },
                d2d,
                dwrite,
                target,
                round,
            })
        }
    }

    /// Draw on `surface`, for a monitor at `dpi`. It starts transparent.
    pub fn draw(&self, surface: &Surface, dpi: u32, f: impl FnOnce(&Canvas)) -> Result<()> {
        let all = RECT {
            left: 0,
            top: 0,
            right: surface.width,
            bottom: surface.height,
        };
        // SAFETY: the surface outlives the drawing; the target is ours.
        unsafe {
            self.target.BindDC(surface.hdc(), &all)?;
            self.target.SetDpi(dpi as f32, dpi as f32);
            self.target
                .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.target.BeginDraw();
            self.target.SetTransform(&Matrix3x2::identity());
            self.target.Clear(Some(&color(Rgb::BLACK, 0.0)));
            f(&Canvas {
                p: self,
                origin: Cell::new((0.0, 0.0)),
            });
            self.target.EndDraw(None, None)
        }
    }

    fn weight(weight: Weight) -> DWRITE_FONT_WEIGHT {
        match weight {
            Weight::Regular => DWRITE_FONT_WEIGHT_NORMAL,
            Weight::Semibold => DWRITE_FONT_WEIGHT_SEMI_BOLD,
            Weight::Bold => DWRITE_FONT_WEIGHT_BOLD,
        }
    }

    /// `runs` (text, stressed or not) laid out in `width` by `height`.
    fn layout(
        &self,
        runs: &[(&str, bool)],
        style: &Text,
        width: f32,
        height: f32,
    ) -> Result<IDWriteTextLayout> {
        let family = if style.size >= 18.0 {
            self.display
        } else {
            self.body
        };
        // SAFETY: valid strings and COM objects of ours.
        unsafe {
            let format = self.dwrite.CreateTextFormat(
                family,
                None::<&IDWriteFontCollection>,
                Self::weight(style.weight),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                style.size,
                w!("en-us"),
            )?;
            format.SetTextAlignment(match style.align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            })?;
            format.SetParagraphAlignment(if style.middle {
                DWRITE_PARAGRAPH_ALIGNMENT_CENTER
            } else {
                DWRITE_PARAGRAPH_ALIGNMENT_NEAR
            })?;
            format.SetWordWrapping(if style.wrap {
                DWRITE_WORD_WRAPPING_WRAP
            } else {
                DWRITE_WORD_WRAPPING_NO_WRAP
            })?;
            let whole: String = runs.iter().map(|(text, _)| *text).collect();
            let layout = self.dwrite.CreateTextLayout(
                &utf16(&whole),
                &format,
                width.max(0.0),
                height.max(0.0),
            )?;
            let mut at = 0u32;
            for (text, stressed) in runs {
                let length = text.encode_utf16().count() as u32;
                if *stressed {
                    layout.SetFontWeight(
                        DWRITE_FONT_WEIGHT_BOLD,
                        DWRITE_TEXT_RANGE {
                            startPosition: at,
                            length,
                        },
                    )?;
                }
                at += length;
            }
            Ok(layout)
        }
    }

    /// The width and height `runs` take, broken to `max_width` if the
    /// style wraps.
    pub fn measure_runs(&self, runs: &[(&str, bool)], style: &Text, max_width: f32) -> (f32, f32) {
        let width = if style.wrap { max_width } else { 100_000.0 };
        let Ok(layout) = self.layout(
            runs,
            &Text {
                middle: false,
                ..*style
            },
            width,
            100_000.0,
        ) else {
            return (0.0, 0.0);
        };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        // SAFETY: an out-parameter of the right type.
        if unsafe { layout.GetMetrics(&mut metrics) }.is_err() {
            return (0.0, 0.0);
        }
        (
            metrics.widthIncludingTrailingWhitespace.ceil(),
            metrics.height.ceil(),
        )
    }

    pub fn measure(&self, text: &str, style: &Text, max_width: f32) -> (f32, f32) {
        self.measure_runs(&[(text, false)], style, max_width)
    }
}

/// Draws on the surface a [`Painter::draw`] is in the middle of.
pub struct Canvas<'a> {
    p: &'a Painter,
    /// Where (0, 0) is for what is drawn now (see [`Canvas::offset`]).
    origin: Cell<(f32, f32)>,
}

impl Canvas<'_> {
    fn brush(&self, c: Rgb, alpha: f32) -> Option<ID2D1SolidColorBrush> {
        // SAFETY: a valid colour; the target is ours.
        unsafe { self.p.target.CreateSolidColorBrush(&color(c, alpha), None) }.ok()
    }

    pub fn painter(&self) -> &Painter {
        self.p
    }

    /// Fill everything.
    pub fn clear(&self, c: Rgb) {
        // SAFETY: a valid colour.
        unsafe { self.p.target.Clear(Some(&color(c, 1.0))) };
    }

    pub fn rect(&self, r: Rect, c: Rgb) {
        if let Some(brush) = self.brush(c, 1.0) {
            // SAFETY: a valid rectangle and brush.
            unsafe { self.p.target.FillRectangle(&r.d2d(), &brush) };
        }
    }

    /// A filled rectangle with rounded corners, `alpha` (0-1) opaque.
    pub fn round_alpha(&self, r: Rect, radius: f32, c: Rgb, alpha: f32) {
        if let Some(brush) = self.brush(c, alpha) {
            let shape = D2D1_ROUNDED_RECT {
                rect: r.d2d(),
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: a valid shape and brush.
            unsafe { self.p.target.FillRoundedRectangle(&shape, &brush) };
        }
    }

    pub fn round(&self, r: Rect, radius: f32, c: Rgb) {
        self.round_alpha(r, radius, c, 1.0);
    }

    /// The outline of a rounded rectangle, `width` wide, drawn inside `r`.
    pub fn outline_alpha(&self, r: Rect, radius: f32, c: Rgb, width: f32, alpha: f32) {
        if let Some(brush) = self.brush(c, alpha) {
            let inner = r.inflate(-width / 2.0);
            let shape = D2D1_ROUNDED_RECT {
                rect: inner.d2d(),
                radiusX: (radius - width / 2.0).max(0.0),
                radiusY: (radius - width / 2.0).max(0.0),
            };
            // SAFETY: a valid shape and brush.
            unsafe {
                self.p
                    .target
                    .DrawRoundedRectangle(&shape, &brush, width, None::<&ID2D1StrokeStyle>)
            };
        }
    }

    pub fn outline(&self, r: Rect, radius: f32, c: Rgb, width: f32) {
        self.outline_alpha(r, radius, c, width, 1.0);
    }

    pub fn circle(&self, cx: f32, cy: f32, radius: f32, c: Rgb) {
        if let Some(brush) = self.brush(c, 1.0) {
            let shape = D2D1_ELLIPSE {
                point: point((cx, cy)),
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: a valid shape and brush.
            unsafe { self.p.target.FillEllipse(&shape, &brush) };
        }
    }

    pub fn line(&self, from: (f32, f32), to: (f32, f32), c: Rgb, width: f32) {
        if let Some(brush) = self.brush(c, 1.0) {
            // SAFETY: valid points, brush and stroke style.
            unsafe {
                self.p
                    .target
                    .DrawLine(point(from), point(to), &brush, width, &self.p.round)
            };
        }
    }

    fn geometry(&self, segs: &[Seg], filled: bool) -> Result<ID2D1PathGeometry> {
        // SAFETY: a geometry of ours, written through its own sink, which
        // is closed before the geometry is used.
        unsafe {
            let geometry = self.p.d2d.CreatePathGeometry()?;
            let sink = geometry.Open()?;
            let begin = if filled {
                D2D1_FIGURE_BEGIN_FILLED
            } else {
                D2D1_FIGURE_BEGIN_HOLLOW
            };
            let mut open = false;
            for seg in segs {
                match *seg {
                    Seg::Move(p) => {
                        if open {
                            sink.EndFigure(D2D1_FIGURE_END_OPEN);
                        }
                        sink.BeginFigure(point(p), begin);
                        open = true;
                    }
                    Seg::Line(p) if open => sink.AddLine(point(p)),
                    Seg::Cubic(a, b, c) if open => sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: point(a),
                        point2: point(b),
                        point3: point(c),
                    }),
                    Seg::Close if open => {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    _ => {}
                }
            }
            if open {
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            }
            sink.Close()?;
            Ok(geometry)
        }
    }

    fn translation((x, y): (f32, f32)) -> Matrix3x2 {
        Matrix3x2 {
            M11: 1.0,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0,
            M31: x,
            M32: y,
        }
    }

    /// Draw in a box of `unit` units a side, shown `size` wide at (x, y).
    fn scaled(&self, x: f32, y: f32, size: f32, unit: f32, f: impl FnOnce()) {
        let k = size / unit;
        let (ox, oy) = self.origin.get();
        let transform = Matrix3x2 {
            M11: k,
            M12: 0.0,
            M21: 0.0,
            M22: k,
            M31: ox + x,
            M32: oy + y,
        };
        // SAFETY: valid matrices; the origin's is restored afterwards.
        unsafe {
            self.p.target.SetTransform(&transform);
            f();
            self.p
                .target
                .SetTransform(&Self::translation(self.origin.get()));
        }
    }

    /// An icon, `size` wide with its top-left corner at (x, y).
    pub fn icon(&self, icon: &Icon, x: f32, y: f32, size: f32, c: Rgb) {
        let Some(brush) = self.brush(c, 1.0) else {
            return;
        };
        self.scaled(x, y, size, brand::icons::SIZE, || {
            for (width, segs) in icon.outlines() {
                if let Ok(geometry) = self.geometry(&segs, false) {
                    // SAFETY: valid geometry, brush and stroke style.
                    unsafe {
                        self.p
                            .target
                            .DrawGeometry(&geometry, &brush, width, &self.p.round)
                    };
                }
            }
        });
    }

    /// The mark, `size` wide at (x, y): the white nail on a tile of `tile`.
    pub fn mark(&self, x: f32, y: f32, size: f32, tile: Rgb) {
        let mark = Mark::of(Variant::for_size(size.round() as u32));
        self.scaled(x, y, size, BOX, || {
            self.round(Rect::new(0.0, 0.0, BOX, BOX), mark.tile_radius, tile);
            let (bx, by, bw, bh, br) = mark.bar;
            self.round(Rect::new(bx, by, bw, bh), br, Rgb::WHITE);
            let mut stem = vec![Seg::Move(mark.stem[0])];
            stem.extend(mark.stem[1..].iter().map(|p| Seg::Line(*p)));
            stem.push(Seg::Close);
            if let (Ok(geometry), Some(brush)) =
                (self.geometry(&stem, true), self.brush(Rgb::WHITE, 1.0))
            {
                // SAFETY: valid geometry and brush.
                unsafe {
                    self.p
                        .target
                        .FillGeometry(&geometry, &brush, None::<&ID2D1Brush>)
                };
            }
        });
    }

    /// A company's logo, fitted into `size` square at (x, y).
    pub fn logo(&self, logo: &Logo, x: f32, y: f32, size: f32) {
        let scale = (size / logo.width as f32).min(size / logo.height as f32);
        let (w, h) = (logo.width as f32 * scale, logo.height as f32 * scale);
        let dest = Rect::new(x + (size - w) / 2.0, y + (size - h) / 2.0, w, h);
        let properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        // SAFETY: `pbgra` holds `height` rows of `width * 4` bytes, as the
        // size and pitch say.
        unsafe {
            let bitmap = self.p.target.CreateBitmap(
                D2D_SIZE_U {
                    width: logo.width,
                    height: logo.height,
                },
                Some(logo.pbgra.as_ptr().cast()),
                logo.width * 4,
                &properties,
            );
            if let Ok(bitmap) = bitmap {
                self.p.target.DrawBitmap(
                    &bitmap,
                    Some(&dest.d2d()),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            }
        }
    }

    /// Text with some runs stressed (bold), in `r`.
    pub fn runs(&self, runs: &[(&str, bool)], r: Rect, style: &Text) {
        let (Ok(layout), Some(brush)) = (
            self.p.layout(runs, style, r.w, r.h),
            self.brush(style.color, 1.0),
        ) else {
            return;
        };
        let options = if style.wrap {
            D2D1_DRAW_TEXT_OPTIONS_NONE
        } else {
            D2D1_DRAW_TEXT_OPTIONS_CLIP
        };
        // SAFETY: a valid layout and brush.
        unsafe {
            self.p
                .target
                .DrawTextLayout(point((r.x, r.y)), &layout, &brush, options)
        };
    }

    pub fn text(&self, text: &str, r: Rect, style: &Text) {
        self.runs(&[(text, false)], r, style);
    }

    /// What `f` draws is moved right by `dx` and down by `dy`.
    pub fn offset(&self, dx: f32, dy: f32, f: impl FnOnce()) {
        let before = self.origin.get();
        self.origin.set((before.0 + dx, before.1 + dy));
        // SAFETY: valid matrices; the one before is restored afterwards.
        unsafe {
            self.p
                .target
                .SetTransform(&Self::translation(self.origin.get()));
            f();
            self.origin.set(before);
            self.p.target.SetTransform(&Self::translation(before));
        }
    }

    /// Only what is inside `r` is drawn by `f`.
    pub fn clipped(&self, r: Rect, f: impl FnOnce()) {
        // SAFETY: a matched push and pop.
        unsafe {
            self.p
                .target
                .PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            f();
            self.p.target.PopAxisAlignedClip();
        }
    }
}
