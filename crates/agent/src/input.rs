//! Platform-neutral input logic: where a normalised mouse position lands on
//! the virtual desktop, and which keys and buttons a session is holding.
//! `crate::win::input` does the actual `SendInput` calls.

use std::collections::BTreeSet;

use protocol::input::{denormalise, scancode, InputEvent, MouseButton};
use protocol::media::MonitorInfo;

/// The bounding box of all monitors, in physical pixels (Windows:
/// `SM_XVIRTUALSCREEN` .. `SM_CYVIRTUALSCREEN` for a per-monitor DPI-aware
/// process, the same space DXGI reports monitor positions in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDesktop {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The pixel on the virtual desktop for normalised `(x, y)` on `monitor`.
pub fn monitor_pixel(monitor: &MonitorInfo, x: u16, y: u16) -> (i32, i32) {
    (
        monitor.x + denormalise(x, monitor.width) as i32,
        monitor.y + denormalise(y, monitor.height) as i32,
    )
}

/// `SendInput` absolute coordinates (`MOUSEEVENTF_ABSOLUTE |
/// MOUSEEVENTF_VIRTUALDESK`) for a pixel on the virtual desktop.
///
/// Windows turns an absolute coordinate `n` back into the pixel
/// `n * extent / 65536` (rounding down), so the smallest `n` that maps to
/// pixel `p` is `ceil(p * 65536 / extent)`. Using exactly that makes the
/// round trip land on the intended pixel, with no off-by-one drift on large
/// or oddly sized desktops.
pub fn absolute(pixel: (i32, i32), desk: VirtualDesktop) -> (i32, i32) {
    fn axis(p: i32, origin: i32, extent: u32) -> i32 {
        if extent == 0 {
            return 0;
        }
        let offset = i64::from(p - origin).clamp(0, i64::from(extent) - 1);
        let extent = i64::from(extent);
        ((offset * 65536 + extent - 1) / extent).min(65535) as i32
    }
    (
        axis(pixel.0, desk.x, desk.width),
        axis(pixel.1, desk.y, desk.height),
    )
}

/// Keys and mouse buttons a technician currently holds down, so they can be
/// released when the session ends. Otherwise a session that drops while
/// Ctrl is down leaves Ctrl stuck for the user.
#[derive(Debug, Default)]
pub struct HeldInput {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<ButtonKey>,
}

/// `MouseButton` in a sortable form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ButtonKey(u8);

impl ButtonKey {
    const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ];

    fn of(button: MouseButton) -> Self {
        Self(Self::ALL.iter().position(|b| *b == button).unwrap_or(0) as u8)
    }

    fn button(self) -> MouseButton {
        Self::ALL[usize::from(self.0)]
    }
}

impl HeldInput {
    pub fn track(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key {
                scancode,
                down: true,
            } => {
                self.keys.insert(scancode);
            }
            InputEvent::Key {
                scancode,
                down: false,
            } => {
                self.keys.remove(&scancode);
            }
            InputEvent::MouseButton { button, down: true } => {
                self.buttons.insert(ButtonKey::of(button));
            }
            InputEvent::MouseButton {
                button,
                down: false,
            } => {
                self.buttons.remove(&ButtonKey::of(button));
            }
            InputEvent::MouseMove { .. } | InputEvent::Wheel { .. } => {}
        }
    }

    pub fn ctrl_held(&self) -> bool {
        self.keys.iter().any(|k| scancode::is_ctrl(*k))
    }

    /// Key-up and button-up events for everything held, and forget it.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let keys = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|scancode| InputEvent::Key {
                scancode,
                down: false,
            });
        let buttons =
            std::mem::take(&mut self.buttons)
                .into_iter()
                .map(|b| InputEvent::MouseButton {
                    button: b.button(),
                    down: false,
                });
        keys.chain(buttons).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::input::{normalise, COORD_MAX};

    fn monitor(x: i32, y: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo {
            id: 0,
            name: "test".into(),
            x,
            y,
            width,
            height,
            primary: false,
        }
    }

    /// What Windows does with an absolute coordinate.
    fn windows_pixel(n: i32, origin: i32, extent: u32) -> i32 {
        origin + (i64::from(n) * i64::from(extent) / 65536) as i32
    }

    #[test]
    fn every_pixel_of_a_large_desktop_round_trips_through_absolute_units() {
        let desk = VirtualDesktop {
            x: -1920,
            y: -120,
            width: 1920 + 3840,
            height: 2160 + 120,
        };
        for px in desk.x..desk.x + desk.width as i32 {
            let (nx, _) = absolute((px, 0), desk);
            assert!((0..=65535).contains(&nx));
            assert_eq!(windows_pixel(nx, desk.x, desk.width), px, "pixel {px}");
        }
        for py in desk.y..desk.y + desk.height as i32 {
            let (_, ny) = absolute((0, py), desk);
            assert_eq!(windows_pixel(ny, desk.y, desk.height), py, "pixel {py}");
        }
    }

    #[test]
    fn corners_of_a_secondary_monitor_left_of_the_primary() {
        // Secondary at negative x (left of the primary), 1280x1024, top-aligned.
        let left = monitor(-1280, 0, 1280, 1024);
        let primary = monitor(0, 0, 2560, 1440);
        let desk = VirtualDesktop {
            x: -1280,
            y: 0,
            width: 1280 + 2560,
            height: 1440,
        };
        assert_eq!(monitor_pixel(&left, 0, 0), (-1280, 0));
        assert_eq!(monitor_pixel(&left, COORD_MAX, COORD_MAX), (-1, 1023));
        assert_eq!(monitor_pixel(&primary, 0, 0), (0, 0));
        assert_eq!(monitor_pixel(&primary, COORD_MAX, COORD_MAX), (2559, 1439));
        // The viewer clicked the centre of a scaled-down picture of `left`.
        let x = normalise(320.0, 640.0 + 1.0);
        let y = normalise(256.0, 512.0 + 1.0);
        let px = monitor_pixel(&left, x, y);
        assert_eq!(px, (-640, 512));
        let (nx, ny) = absolute(px, desk);
        assert_eq!(windows_pixel(nx, desk.x, desk.width), -640);
        assert_eq!(windows_pixel(ny, desk.y, desk.height), 512);
    }

    #[test]
    fn positions_off_the_desktop_are_clamped() {
        let desk = VirtualDesktop {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(absolute((-50, -50), desk), (0, 0));
        let (nx, ny) = absolute((5000, 5000), desk);
        assert_eq!(windows_pixel(nx, 0, 1920), 1919);
        assert_eq!(windows_pixel(ny, 0, 1080), 1079);
    }

    #[test]
    fn held_keys_and_buttons_are_released_once() {
        let mut held = HeldInput::default();
        for e in [
            InputEvent::Key {
                scancode: scancode::LEFT_CTRL,
                down: true,
            },
            InputEvent::Key {
                scancode: 0x1E, // A
                down: true,
            },
            InputEvent::Key {
                scancode: 0x1E,
                down: false,
            },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
            },
            // Autorepeat: a second down for the same key.
            InputEvent::Key {
                scancode: scancode::LEFT_CTRL,
                down: true,
            },
        ] {
            held.track(&e);
        }
        assert!(held.ctrl_held());
        assert_eq!(
            held.release_all(),
            [
                InputEvent::Key {
                    scancode: scancode::LEFT_CTRL,
                    down: false
                },
                InputEvent::MouseButton {
                    button: MouseButton::Right,
                    down: false
                },
            ]
        );
        assert!(!held.ctrl_held());
        assert_eq!(held.release_all(), []);
    }
}
