//! SVG path data (`M12 3l7 3v5c0 4.5-3 8.3-7 10z`) as plain segments, so
//! the same icon definitions serve the web pages (as SVG) and the Windows
//! UI (as Direct2D geometry). Arcs become cubic curves.

pub type Point = (f32, f32);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seg {
    Move(Point),
    Line(Point),
    /// Two control points, then the end point.
    Cubic(Point, Point, Point),
    Close,
}

struct Lexer<'a> {
    rest: &'a [u8],
}

impl Lexer<'_> {
    fn skip(&mut self) {
        while let [b' ' | b',' | b'\n' | b'\t' | b'\r', rest @ ..] = self.rest {
            self.rest = rest;
        }
    }

    /// The next command letter, if one is next.
    fn command(&mut self) -> Option<u8> {
        self.skip();
        match self.rest {
            [c, rest @ ..] if c.is_ascii_alphabetic() => {
                self.rest = rest;
                Some(*c)
            }
            _ => None,
        }
    }

    fn at_number(&mut self) -> bool {
        self.skip();
        matches!(self.rest, [b'0'..=b'9' | b'-' | b'+' | b'.', ..])
    }

    fn number(&mut self) -> Result<f32, String> {
        self.skip();
        let mut len = 0;
        let mut dot = false;
        while let Some(&c) = self.rest.get(len) {
            let sign = (c == b'-' || c == b'+') && len == 0;
            let point = c == b'.' && !dot;
            if !(c.is_ascii_digit() || sign || point) {
                break;
            }
            dot |= c == b'.';
            len += 1;
        }
        let (text, rest) = self.rest.split_at(len);
        self.rest = rest;
        std::str::from_utf8(text)
            .ok()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| format!("expected a number at {:?}", String::from_utf8_lossy(rest)))
    }

    /// An arc's flag: a single `0` or `1`, which may run into what follows.
    fn flag(&mut self) -> Result<bool, String> {
        self.skip();
        match self.rest {
            [c @ (b'0' | b'1'), rest @ ..] => {
                self.rest = rest;
                Ok(*c == b'1')
            }
            _ => Err("expected an arc flag".into()),
        }
    }

    fn point(&mut self) -> Result<Point, String> {
        Ok((self.number()?, self.number()?))
    }
}

/// The segments of SVG path data `d`, in absolute coordinates.
pub fn parse(d: &str) -> Result<Vec<Seg>, String> {
    let mut lexer = Lexer { rest: d.as_bytes() };
    let mut out = Vec::new();
    let (mut at, mut start) = ((0.0, 0.0), (0.0, 0.0));
    // The last cubic's second control point, for `s`.
    let mut last_control: Option<Point> = None;
    let mut command = lexer
        .command()
        .ok_or("path data must start with a command")?;
    loop {
        let relative = command.is_ascii_lowercase();
        let abs = |p: Point, at: Point| {
            if relative {
                (at.0 + p.0, at.1 + p.1)
            } else {
                p
            }
        };
        let mut control = None;
        match command.to_ascii_uppercase() {
            b'M' => {
                at = abs(lexer.point()?, at);
                start = at;
                out.push(Seg::Move(at));
                // Further pairs are lines.
                command = if relative { b'l' } else { b'L' };
                if lexer.at_number() {
                    continue;
                }
            }
            b'L' => {
                at = abs(lexer.point()?, at);
                out.push(Seg::Line(at));
            }
            b'H' => {
                let x = lexer.number()?;
                at = (if relative { at.0 + x } else { x }, at.1);
                out.push(Seg::Line(at));
            }
            b'V' => {
                let y = lexer.number()?;
                at = (at.0, if relative { at.1 + y } else { y });
                out.push(Seg::Line(at));
            }
            b'C' => {
                let (c1, c2, to) = (lexer.point()?, lexer.point()?, lexer.point()?);
                let (c1, c2, to) = (abs(c1, at), abs(c2, at), abs(to, at));
                out.push(Seg::Cubic(c1, c2, to));
                control = Some(c2);
                at = to;
            }
            b'S' => {
                let (c2, to) = (lexer.point()?, lexer.point()?);
                let (c2, to) = (abs(c2, at), abs(to, at));
                // The first control point mirrors the last curve's second.
                let c1 = match last_control {
                    Some(c) => (2.0 * at.0 - c.0, 2.0 * at.1 - c.1),
                    None => at,
                };
                out.push(Seg::Cubic(c1, c2, to));
                control = Some(c2);
                at = to;
            }
            b'A' => {
                let (rx, ry, rotation) = (lexer.number()?, lexer.number()?, lexer.number()?);
                let (large, sweep) = (lexer.flag()?, lexer.flag()?);
                let to = abs(lexer.point()?, at);
                arc(at, (rx, ry), rotation, large, sweep, to, &mut out);
                at = to;
            }
            b'Z' => {
                out.push(Seg::Close);
                at = start;
            }
            other => return Err(format!("unsupported path command {:?}", other as char)),
        }
        last_control = control;
        if command.eq_ignore_ascii_case(&b'Z') || !lexer.at_number() {
            match lexer.command() {
                Some(next) => command = next,
                None => break,
            }
        }
    }
    lexer.skip();
    if lexer.rest.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "unexpected {:?} in path data",
            String::from_utf8_lossy(lexer.rest)
        ))
    }
}

/// An elliptical arc from `from` to `to` as cubic curves (the SVG
/// specification's endpoint-to-centre conversion).
fn arc(
    from: Point,
    radii: (f32, f32),
    rotation_deg: f32,
    large: bool,
    sweep: bool,
    to: Point,
    out: &mut Vec<Seg>,
) {
    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if from == to {
        return;
    }
    if rx == 0.0 || ry == 0.0 {
        out.push(Seg::Line(to));
        return;
    }
    let (sin, cos) = rotation_deg.to_radians().sin_cos();
    let (dx, dy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    // Radii too small to span the two points are scaled up.
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        rx *= lambda.sqrt();
        ry *= lambda.sqrt();
    }
    let num = rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1;
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    let mut coef = (num / den).max(0.0).sqrt();
    if large == sweep {
        coef = -coef;
    }
    let (cxp, cyp) = (coef * rx * y1 / ry, -coef * ry * x1 / rx);
    let cx = cos * cxp - sin * cyp + (from.0 + to.0) / 2.0;
    let cy = sin * cxp + cos * cyp + (from.1 + to.1) / 2.0;
    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let (ux, uy) = ((x1 - cxp) / rx, (y1 - cyp) / ry);
    let (vx, vy) = ((-x1 - cxp) / rx, (-y1 - cyp) / ry);
    let theta = angle(1.0, 0.0, ux, uy);
    let mut delta = angle(ux, uy, vx, vy);
    let turn = std::f32::consts::TAU;
    if !sweep && delta > 0.0 {
        delta -= turn;
    } else if sweep && delta < 0.0 {
        delta += turn;
    }
    let pieces = (delta.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as u32;
    let step = delta / pieces as f32;
    let t = 4.0 / 3.0 * (step / 4.0).tan();
    let place = |x: f32, y: f32| {
        (
            cx + rx * x * cos - ry * y * sin,
            cy + rx * x * sin + ry * y * cos,
        )
    };
    for i in 0..pieces {
        let a1 = theta + i as f32 * step;
        let a2 = a1 + step;
        let (s1, c1) = a1.sin_cos();
        let (s2, c2) = a2.sin_cos();
        let end = if i + 1 == pieces { to } else { place(c2, s2) };
        out.push(Seg::Cubic(
            place(c1 - t * s1, s1 + t * c1),
            place(c2 + t * s2, s2 - t * c2),
            end,
        ));
    }
}

/// A rectangle with rounded corners, as segments.
pub fn round_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Vec<Seg> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let d = format!(
        "M{} {}h{}a{r} {r} 0 0 1 {r} {r}v{}a{r} {r} 0 0 1 -{r} {r}h-{}a{r} {r} 0 0 1 -{r} -{r}v-{}a{r} {r} 0 0 1 {r} -{r}z",
        x + r,
        y,
        w - 2.0 * r,
        h - 2.0 * r,
        w - 2.0 * r,
        h - 2.0 * r,
    );
    parse(&d).expect("well-formed")
}

/// A circle, as segments.
pub fn circle(cx: f32, cy: f32, r: f32) -> Vec<Seg> {
    let d = format!(
        "M{} {cy}a{r} {r} 0 1 0 {} 0a{r} {r} 0 1 0 -{} 0z",
        cx - r,
        2.0 * r,
        2.0 * r
    );
    parse(&d).expect("well-formed")
}

/// Segments back as SVG path data (absolute commands).
pub fn to_svg(segs: &[Seg]) -> String {
    let n = |v: f32| {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    };
    segs.iter()
        .map(|seg| match seg {
            Seg::Move(p) => format!("M{} {}", n(p.0), n(p.1)),
            Seg::Line(p) => format!("L{} {}", n(p.0), n(p.1)),
            Seg::Cubic(a, b, c) => format!(
                "C{} {} {} {} {} {}",
                n(a.0),
                n(a.1),
                n(b.0),
                n(b.1),
                n(c.0),
                n(c.1)
            ),
            Seg::Close => "Z".to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Point, b: Point) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
    }

    #[test]
    fn relative_and_implicit_commands() {
        // The mark's stem, as the brand sheet writes it.
        let segs = parse("M27.5 22h9v19l-4.5 11-4.5-11z").unwrap();
        assert_eq!(
            segs,
            [
                Seg::Move((27.5, 22.0)),
                Seg::Line((36.5, 22.0)),
                Seg::Line((36.5, 41.0)),
                Seg::Line((32.0, 52.0)),
                Seg::Line((27.5, 41.0)),
                Seg::Close,
            ]
        );
        // Numbers run together: `h.01`, and a move followed by pairs.
        let dots = parse("M7.5 12h.01M12 12h.01").unwrap();
        assert_eq!(dots.len(), 4);
        assert_eq!(dots[1], Seg::Line((7.51, 12.0)));
        let lines = parse("M0 0 1 1 2 0").unwrap();
        assert_eq!(lines[2], Seg::Line((2.0, 0.0)));
    }

    #[test]
    fn curves_and_smooth_curves() {
        // The eye: two smooth curves mirror the control points before.
        let eye = parse("M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z").unwrap();
        assert_eq!(eye.len(), 6);
        let Seg::Cubic(c1, c2, to) = eye[2] else {
            panic!("{:?}", eye[2])
        };
        // After `s3.5-7 10-7` (control 5.5,5 to 12,5), the next starts
        // with that control mirrored about 12,5.
        assert!(close(c1, (18.5, 5.0)) && close(c2, (22.0, 12.0)) && close(to, (22.0, 12.0)));
        assert_eq!(eye[5], Seg::Close);
    }

    #[test]
    fn arcs_become_curves_that_end_where_they_should() {
        let segs = circle(12.0, 12.0, 9.0);
        // Two half turns, each two quarter curves.
        assert_eq!(segs.len(), 6);
        for seg in &segs {
            if let Seg::Cubic(_, _, to) = seg {
                let d = ((to.0 - 12.0).powi(2) + (to.1 - 12.0).powi(2)).sqrt();
                assert!((d - 9.0).abs() < 0.01, "{to:?}");
            }
        }
        // Flags packed against the next number, as minifiers write them.
        let packed = parse("M8 11V7a4 4 0 018 0v4").unwrap();
        let Seg::Cubic(_, _, end) = packed[packed.len() - 2] else {
            panic!()
        };
        assert!(close(end, (16.0, 7.0)));
        let rect = round_rect(3.0, 7.0, 18.0, 10.0, 2.0);
        assert_eq!(rect[0], Seg::Move((5.0, 7.0)));
        assert_eq!(rect.last(), Some(&Seg::Close));
    }

    #[test]
    fn nonsense_is_an_error() {
        for bad in [
            "",
            "12 3",
            "M1",
            "M1 2 Q3 4 5 6",
            "M1 2 L",
            "M0 0a1 1 0 2 0 1 1",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn segments_write_back_as_path_data() {
        let d = to_svg(&parse("M1 2l3 0c0 1 1 2 2 2z").unwrap());
        assert_eq!(d, "M1 2L4 2C4 3 5 4 6 4Z");
    }
}
