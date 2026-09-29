//! Remote input: what the viewer captures and the agent's helper injects.
//!
//! Keys travel as PC scancodes (set 1), the physical key position, which is
//! what `SendInput` with `KEYEVENTF_SCANCODE` takes. The agent's keyboard
//! layout then decides which character a key produces, as if the technician
//! typed on a keyboard plugged into the remote machine. Extended keys (arrows,
//! right Ctrl, the Windows keys, ...) carry the `0xE0` prefix in the high
//! byte, e.g. `0xE04B` for Left.
//!
//! Mouse positions are relative to one monitor, normalised to `0..=65535`
//! across its width and height, so they do not depend on the size the video
//! was scaled to or on the viewer's window. The helper maps them onto the
//! remote virtual desktop (see `agent::input`).

use serde::{Deserialize, Serialize};

/// Largest normalised coordinate (right or bottom edge).
pub const COORD_MAX: u16 = u16::MAX;

/// One notch of a mouse wheel, in the units of [`InputEvent::Wheel`]
/// (Windows' `WHEEL_DELTA`).
pub const WHEEL_NOTCH: i16 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    /// Browser back (XBUTTON1).
    Back,
    /// Browser forward (XBUTTON2).
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Move the pointer to `(x, y)` on `monitor`, each `0..=COORD_MAX`.
    MouseMove {
        monitor: u32,
        x: u16,
        y: u16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Scroll; positive `dy` is away from the user (up), positive `dx` right.
    /// [`WHEEL_NOTCH`] per notch; smaller values for smooth scrolling.
    Wheel {
        dx: i16,
        dy: i16,
    },
    /// A key went down or up. `scancode` as described in the module docs.
    Key {
        scancode: u16,
        down: bool,
    },
}

/// Scancodes the agent needs to know about.
pub mod scancode {
    pub const LEFT_CTRL: u16 = 0x1D;
    pub const RIGHT_CTRL: u16 = 0xE01D;
    pub const LEFT_SHIFT: u16 = 0x2A;
    pub const RIGHT_SHIFT: u16 = 0x36;
    pub const LEFT_ALT: u16 = 0x38;
    pub const RIGHT_ALT: u16 = 0xE038;
    pub const LEFT_META: u16 = 0xE05B;
    pub const RIGHT_META: u16 = 0xE05C;
    pub const F12: u16 = 0x58;

    /// Whether `code` has the `0xE0` extended prefix.
    pub fn is_extended(code: u16) -> bool {
        code >> 8 == 0xE0
    }

    pub fn is_ctrl(code: u16) -> bool {
        code == LEFT_CTRL || code == RIGHT_CTRL
    }
}

/// Map `pos` in `0..extent` (e.g. a pixel column of a picture `extent`
/// pixels wide) to `0..=COORD_MAX`. Out-of-range positions are clamped.
pub fn normalise(pos: f64, extent: f64) -> u16 {
    if extent <= 1.0 {
        return 0;
    }
    let t = (pos / (extent - 1.0)).clamp(0.0, 1.0);
    (t * f64::from(COORD_MAX)).round() as u16
}

/// Inverse of [`normalise`]: the pixel in `0..extent` for `value`.
pub fn denormalise(value: u16, extent: u32) -> u32 {
    if extent == 0 {
        return 0;
    }
    let max = u64::from(extent - 1);
    ((u64::from(value) * max + u64::from(COORD_MAX) / 2) / u64::from(COORD_MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    #[test]
    fn input_events_round_trip_through_postcard() {
        let events = [
            InputEvent::MouseMove {
                monitor: 1,
                x: 0,
                y: COORD_MAX,
            },
            InputEvent::MouseButton {
                button: MouseButton::Forward,
                down: true,
            },
            InputEvent::Wheel {
                dx: -WHEEL_NOTCH,
                dy: 3 * WHEEL_NOTCH,
            },
            InputEvent::Key {
                scancode: scancode::RIGHT_CTRL,
                down: false,
            },
        ];
        for event in events {
            for msg in [
                Message::Input(event),
                Message::SessionInput {
                    session_id: 7,
                    event,
                },
            ] {
                let bytes = postcard::to_stdvec(&msg).unwrap();
                assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
            }
        }
    }

    #[test]
    fn input_messages_are_small() {
        // Mouse moves are the most frequent message; keep them tiny.
        let bytes = postcard::to_stdvec(&Message::Input(InputEvent::MouseMove {
            monitor: 0,
            x: 40_000,
            y: 20_000,
        }))
        .unwrap();
        assert!(bytes.len() <= 10, "{} bytes", bytes.len());
    }

    #[test]
    fn normalise_maps_edges_to_the_full_range() {
        assert_eq!(normalise(0.0, 1920.0), 0);
        assert_eq!(normalise(1919.0, 1920.0), COORD_MAX);
        assert_eq!(normalise(-5.0, 1920.0), 0);
        assert_eq!(normalise(5000.0, 1920.0), COORD_MAX);
        assert_eq!(normalise(3.0, 1.0), 0);
    }

    #[test]
    fn denormalise_inverts_normalise_for_every_pixel() {
        for extent in [1u32, 2, 768, 1080, 1920, 3840] {
            for px in 0..extent {
                let n = normalise(f64::from(px), f64::from(extent));
                assert_eq!(denormalise(n, extent), px, "extent {extent} px {px}");
            }
        }
    }

    #[test]
    fn extended_and_ctrl_scancodes() {
        assert!(scancode::is_extended(scancode::RIGHT_CTRL));
        assert!(scancode::is_extended(scancode::LEFT_META));
        assert!(!scancode::is_extended(scancode::LEFT_CTRL));
        assert!(scancode::is_ctrl(scancode::LEFT_CTRL) && scancode::is_ctrl(scancode::RIGHT_CTRL));
        assert!(!scancode::is_ctrl(scancode::LEFT_ALT));
    }
}
