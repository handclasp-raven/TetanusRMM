//! The mark: a T that is a nail, white on a rounded tile.
//!
//! The brand sheet draws it three ways, heavier as it gets smaller so that
//! it still reads at 16 pixels. All are in a 64-unit box.

use crate::path::Point;

/// The box the mark is drawn in.
pub const BOX: f32 = 64.0;

/// Which drawing of the mark suits a size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// 16 px and the tray: the heaviest.
    Small,
    /// 24 and 32 px.
    Medium,
    /// 48 px and up, and the logo.
    Large,
}

impl Variant {
    /// The drawing for an icon `px` pixels square.
    pub fn for_size(px: u32) -> Self {
        match px {
            0..=20 => Variant::Small,
            21..=40 => Variant::Medium,
            _ => Variant::Large,
        }
    }
}

/// The mark's shapes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mark {
    /// Corner radius of the tile (which fills the box).
    pub tile_radius: f32,
    /// The crossbar: x, y, width, height, corner radius.
    pub bar: (f32, f32, f32, f32, f32),
    /// The stem, tapering to the nail's point.
    pub stem: [Point; 5],
}

impl Mark {
    pub const fn of(variant: Variant) -> Self {
        match variant {
            Variant::Large => Mark {
                tile_radius: 14.0,
                bar: (14.0, 13.0, 36.0, 9.0, 3.0),
                stem: [
                    (27.5, 22.0),
                    (36.5, 22.0),
                    (36.5, 41.0),
                    (32.0, 52.0),
                    (27.5, 41.0),
                ],
            },
            Variant::Medium => Mark {
                tile_radius: 14.0,
                bar: (12.0, 11.0, 40.0, 11.0, 3.0),
                stem: [
                    (26.5, 22.0),
                    (37.5, 22.0),
                    (37.5, 40.0),
                    (32.0, 53.0),
                    (26.5, 40.0),
                ],
            },
            Variant::Small => Mark {
                tile_radius: 12.0,
                bar: (10.0, 10.0, 44.0, 13.0, 2.0),
                stem: [
                    (25.0, 23.0),
                    (39.0, 23.0),
                    (39.0, 40.0),
                    (32.0, 54.0),
                    (25.0, 40.0),
                ],
            },
        }
    }
}

/// What the tray icon says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayState {
    /// Connected to the server; nobody is in a session.
    Connected,
    /// A technician is connected to this machine.
    Live,
    /// Not connected to the server (or not enrolled).
    Offline,
}

/// The tray icon's status badge, bottom right: the hole cut for it, and
/// the dot in the hole (centre x, centre y, radius).
pub const BADGE_HOLE: (f32, f32, f32) = (52.0, 52.0, 12.0);
pub const BADGE_DOT: (f32, f32, f32) = (52.0, 52.0, 8.0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smaller_icons_get_the_heavier_mark() {
        assert_eq!(Variant::for_size(16), Variant::Small);
        assert_eq!(Variant::for_size(24), Variant::Medium);
        assert_eq!(Variant::for_size(32), Variant::Medium);
        assert_eq!(Variant::for_size(48), Variant::Large);
        assert_eq!(Variant::for_size(256), Variant::Large);
        let bar = |v| Mark::of(v).bar;
        assert!(bar(Variant::Small).3 > bar(Variant::Medium).3);
        assert!(bar(Variant::Medium).3 > bar(Variant::Large).3);
    }

    #[test]
    fn every_drawing_is_centred_and_meets_its_crossbar() {
        for variant in [Variant::Small, Variant::Medium, Variant::Large] {
            let m = Mark::of(variant);
            let (x, y, w, h, _) = m.bar;
            assert_eq!(x + w / 2.0, BOX / 2.0);
            // The stem starts where the bar ends, and its point is centred.
            assert_eq!(m.stem[0].1, y + h);
            assert_eq!(m.stem[3].0, BOX / 2.0);
            assert_eq!((m.stem[0].0 + m.stem[1].0) / 2.0, BOX / 2.0);
        }
    }
}
