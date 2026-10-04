//! The dialog icons: 24 px, stroked 1.75 wide with round caps and joins.
//!
//! The first six are the brand sheet's, shape for shape. The rest are not
//! on the sheet but appear on the artboards (or are needed beside them),
//! and are drawn in the same style.

use crate::path::{self, Seg};

/// The box every icon is drawn in.
pub const SIZE: f32 = 24.0;
/// The stroke width, in that box.
pub const STROKE: f32 = 1.75;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    Path(&'static str),
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
    },
    Circle {
        cx: f32,
        cy: f32,
        r: f32,
    },
}

/// One stroked shape of an icon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub shape: Shape,
    pub width: f32,
}

const fn path(d: &'static str) -> Stroke {
    Stroke {
        shape: Shape::Path(d),
        width: STROKE,
    }
}

const fn rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Stroke {
    Stroke {
        shape: Shape::Rect { x, y, w, h, r },
        width: STROKE,
    }
}

const fn circle(cx: f32, cy: f32, r: f32) -> Stroke {
    Stroke {
        shape: Shape::Circle { cx, cy, r },
        width: STROKE,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Icon {
    pub name: &'static str,
    pub strokes: &'static [Stroke],
}

/// A shield with a check: the scam warning.
pub const SAFETY: Icon = Icon {
    name: "safety",
    strokes: &[
        path("M12 3l7 3v5c0 4.5-3 8.3-7 10-4-1.7-7-5.5-7-10V6z"),
        path("M9 12l2 2 4-4"),
    ],
};

/// A field of dots: the support code.
pub const CODE: Icon = Icon {
    name: "code",
    strokes: &[
        rect(3.0, 7.0, 18.0, 10.0, 2.0),
        Stroke {
            shape: Shape::Path("M7.5 12h.01M12 12h.01M16.5 12h.01"),
            width: 2.6,
        },
    ],
};

/// A screen with a pointer: remote control.
pub const REMOTE: Icon = Icon {
    name: "remote",
    strokes: &[
        rect(3.0, 4.0, 18.0, 12.0, 2.0),
        path("M8 20h8M12 16v4"),
        path("M10 7.5l5 2.2-2.2.8-.8 2.2z"),
    ],
};

/// A key: the lent password.
pub const PASSWORD: Icon = Icon {
    name: "password",
    strokes: &[circle(8.0, 15.0, 4.0), path("M11 12l8-8M16 7l2 2M14 9l2 2")],
};

/// An eye: seeing the screen; showing what is typed.
pub const VIEW: Icon = Icon {
    name: "view",
    strokes: &[
        path("M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"),
        circle(12.0, 12.0, 3.0),
    ],
};

/// A stop button: ending a session.
pub const END: Icon = Icon {
    name: "end",
    strokes: &[circle(12.0, 12.0, 9.0), rect(9.0, 9.0, 6.0, 6.0, 1.0)],
};

/// The eye struck through: hiding what is typed.
pub const VIEW_OFF: Icon = Icon {
    name: "view-off",
    strokes: &[
        path("M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"),
        circle(12.0, 12.0, 3.0),
        path("M4 4l16 16"),
    ],
};

/// A struck-through circle: what a real business never does.
pub const PROHIBITED: Icon = Icon {
    name: "prohibited",
    strokes: &[circle(12.0, 12.0, 9.0), path("M5.65 5.65l12.7 12.7")],
};

/// A telephone handset: an unexpected call.
pub const PHONE: Icon = Icon {
    name: "phone",
    strokes: &[path(
        "M5 4h3.2l1.6 4.2-2 1.5a11 11 0 0 0 6.5 6.5l1.5-2 4.2 1.6V19a2 2 0 0 1-2 2A15 15 0 0 1 3 6a2 2 0 0 1 2-2z",
    )],
};

/// A padlock: nothing is seen until it is approved; Quit is locked.
pub const LOCK: Icon = Icon {
    name: "lock",
    strokes: &[
        rect(5.0, 11.0, 14.0, 10.0, 2.0),
        path("M8 11V7a4 4 0 0 1 8 0v4"),
    ],
};

/// A check mark: an assurance.
pub const CHECK: Icon = Icon {
    name: "check",
    strokes: &[path("M5 12.5l4.5 4.5L19 7.5")],
};

/// An encircled i: About.
pub const INFO: Icon = Icon {
    name: "info",
    strokes: &[
        circle(12.0, 12.0, 9.0),
        path("M12 11v5.5"),
        Stroke {
            shape: Shape::Path("M12 7.75h.01"),
            width: 2.2,
        },
    ],
};

/// A cross: close; end the session from the collapsed pill.
pub const CLOSE: Icon = Icon {
    name: "close",
    strokes: &[path("M6.5 6.5l11 11M17.5 6.5l-11 11")],
};

pub const ALL: [Icon; 13] = [
    SAFETY, CODE, REMOTE, PASSWORD, VIEW, END, VIEW_OFF, PROHIBITED, PHONE, LOCK, CHECK, INFO,
    CLOSE,
];

impl Stroke {
    /// The shape as segments, to stroke.
    pub fn outline(&self) -> Vec<Seg> {
        match self.shape {
            Shape::Path(d) => path::parse(d).expect("icon paths are well-formed"),
            Shape::Rect { x, y, w, h, r } => path::round_rect(x, y, w, h, r),
            Shape::Circle { cx, cy, r } => path::circle(cx, cy, r),
        }
    }
}

impl Icon {
    /// Each stroke's width and outline.
    pub fn outlines(&self) -> Vec<(f32, Vec<Seg>)> {
        self.strokes
            .iter()
            .map(|s| (s.width, s.outline()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_and_stays_inside_its_box() {
        for icon in ALL {
            for (width, segs) in icon.outlines() {
                assert!(!segs.is_empty(), "{}", icon.name);
                let reach = width / 2.0;
                let inside = |p: (f32, f32)| {
                    p.0 - reach >= 0.0
                        && p.1 - reach >= 0.0
                        && p.0 + reach <= SIZE
                        && p.1 + reach <= SIZE
                };
                for seg in segs {
                    let end = match seg {
                        Seg::Move(p) | Seg::Line(p) | Seg::Cubic(_, _, p) => p,
                        Seg::Close => continue,
                    };
                    assert!(inside(end), "{}: {end:?}", icon.name);
                }
            }
        }
    }

    #[test]
    fn names_are_distinct() {
        let mut names: Vec<&str> = ALL.iter().map(|i| i.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
    }
}
