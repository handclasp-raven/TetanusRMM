//! Physical keys (winit `KeyCode`, the key's position on the keyboard) to
//! PC set-1 scancodes as `SendInput` takes them. Extended keys carry `0xE0`
//! in the high byte. Values follow the Windows column of Chromium's
//! `dom_code_data.inc`.
//!
//! Sending positions rather than characters means the remote keyboard layout
//! decides what a key types, exactly as with a physical keyboard plugged into
//! the remote machine, and shortcuts like Ctrl+C work in every layout.

use protocol::input::scancode as sc;
use winit::keyboard::KeyCode;

/// The scancode for `code`, or `None` for keys with no PC equivalent.
pub fn scancode(code: KeyCode) -> Option<u16> {
    use KeyCode::*;
    let s = match code {
        Escape => 0x01,
        Digit1 => 0x02,
        Digit2 => 0x03,
        Digit3 => 0x04,
        Digit4 => 0x05,
        Digit5 => 0x06,
        Digit6 => 0x07,
        Digit7 => 0x08,
        Digit8 => 0x09,
        Digit9 => 0x0A,
        Digit0 => 0x0B,
        Minus => 0x0C,
        Equal => 0x0D,
        Backspace => 0x0E,
        Tab => 0x0F,
        KeyQ => 0x10,
        KeyW => 0x11,
        KeyE => 0x12,
        KeyR => 0x13,
        KeyT => 0x14,
        KeyY => 0x15,
        KeyU => 0x16,
        KeyI => 0x17,
        KeyO => 0x18,
        KeyP => 0x19,
        BracketLeft => 0x1A,
        BracketRight => 0x1B,
        Enter => 0x1C,
        ControlLeft => sc::LEFT_CTRL,
        KeyA => 0x1E,
        KeyS => 0x1F,
        KeyD => 0x20,
        KeyF => 0x21,
        KeyG => 0x22,
        KeyH => 0x23,
        KeyJ => 0x24,
        KeyK => 0x25,
        KeyL => 0x26,
        Semicolon => 0x27,
        Quote => 0x28,
        Backquote => 0x29,
        ShiftLeft => sc::LEFT_SHIFT,
        Backslash => 0x2B,
        KeyZ => 0x2C,
        KeyX => 0x2D,
        KeyC => 0x2E,
        KeyV => 0x2F,
        KeyB => 0x30,
        KeyN => 0x31,
        KeyM => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        ShiftRight => sc::RIGHT_SHIFT,
        NumpadMultiply => 0x37,
        AltLeft => sc::LEFT_ALT,
        Space => 0x39,
        CapsLock => 0x3A,
        F1 => 0x3B,
        F2 => 0x3C,
        F3 => 0x3D,
        F4 => 0x3E,
        F5 => 0x3F,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        ScrollLock => 0x46,
        Numpad7 => 0x47,
        Numpad8 => 0x48,
        Numpad9 => 0x49,
        NumpadSubtract => 0x4A,
        Numpad4 => 0x4B,
        Numpad5 => 0x4C,
        Numpad6 => 0x4D,
        NumpadAdd => 0x4E,
        Numpad1 => 0x4F,
        Numpad2 => 0x50,
        Numpad3 => 0x51,
        Numpad0 => 0x52,
        NumpadDecimal => 0x53,
        IntlBackslash => 0x56,
        F11 => 0x57,
        F12 => sc::F12,
        NumpadEqual => 0x59,
        KanaMode => 0x70,
        IntlRo => 0x73,
        Convert => 0x79,
        NonConvert => 0x7B,
        IntlYen => 0x7D,
        NumpadComma => 0x7E,
        NumpadEnter => 0xE01C,
        ControlRight => sc::RIGHT_CTRL,
        NumpadDivide => 0xE035,
        PrintScreen => 0xE037,
        AltRight => sc::RIGHT_ALT,
        NumLock => 0xE045,
        Home => 0xE047,
        ArrowUp => 0xE048,
        PageUp => 0xE049,
        ArrowLeft => 0xE04B,
        ArrowRight => 0xE04D,
        End => 0xE04F,
        ArrowDown => 0xE050,
        PageDown => 0xE051,
        Insert => 0xE052,
        Delete => 0xE053,
        SuperLeft => sc::LEFT_META,
        SuperRight => sc::RIGHT_META,
        ContextMenu => 0xE05D,
        // Pause is a special multi-byte sequence (E1 1D 45) that SendInput
        // cannot express as one scancode; media and other keys have none.
        _ => return None,
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Every mapped key, for the uniqueness check.
    const KEYS: &[KeyCode] = {
        use KeyCode::*;
        &[
            Escape,
            Digit1,
            Digit2,
            Digit3,
            Digit4,
            Digit5,
            Digit6,
            Digit7,
            Digit8,
            Digit9,
            Digit0,
            Minus,
            Equal,
            Backspace,
            Tab,
            KeyQ,
            KeyW,
            KeyE,
            KeyR,
            KeyT,
            KeyY,
            KeyU,
            KeyI,
            KeyO,
            KeyP,
            BracketLeft,
            BracketRight,
            Enter,
            ControlLeft,
            KeyA,
            KeyS,
            KeyD,
            KeyF,
            KeyG,
            KeyH,
            KeyJ,
            KeyK,
            KeyL,
            Semicolon,
            Quote,
            Backquote,
            ShiftLeft,
            Backslash,
            KeyZ,
            KeyX,
            KeyC,
            KeyV,
            KeyB,
            KeyN,
            KeyM,
            Comma,
            Period,
            Slash,
            ShiftRight,
            NumpadMultiply,
            AltLeft,
            Space,
            CapsLock,
            F1,
            F2,
            F3,
            F4,
            F5,
            F6,
            F7,
            F8,
            F9,
            F10,
            ScrollLock,
            Numpad7,
            Numpad8,
            Numpad9,
            NumpadSubtract,
            Numpad4,
            Numpad5,
            Numpad6,
            NumpadAdd,
            Numpad1,
            Numpad2,
            Numpad3,
            Numpad0,
            NumpadDecimal,
            IntlBackslash,
            F11,
            F12,
            NumpadEqual,
            KanaMode,
            IntlRo,
            Convert,
            NonConvert,
            IntlYen,
            NumpadComma,
            NumpadEnter,
            ControlRight,
            NumpadDivide,
            PrintScreen,
            AltRight,
            NumLock,
            Home,
            ArrowUp,
            PageUp,
            ArrowLeft,
            ArrowRight,
            End,
            ArrowDown,
            PageDown,
            Insert,
            Delete,
            SuperLeft,
            SuperRight,
            ContextMenu,
        ]
    };

    #[test]
    fn no_two_keys_share_a_scancode() {
        let mut seen = HashMap::new();
        for &key in KEYS {
            let code = scancode(key).unwrap_or_else(|| panic!("{key:?} unmapped"));
            if let Some(other) = seen.insert(code, key) {
                panic!("{key:?} and {other:?} both map to {code:#x}");
            }
        }
    }

    #[test]
    fn well_known_keys() {
        assert_eq!(scancode(KeyCode::KeyA), Some(0x1E));
        assert_eq!(scancode(KeyCode::KeyC), Some(0x2E));
        assert_eq!(scancode(KeyCode::Enter), Some(0x1C));
        assert_eq!(scancode(KeyCode::NumpadEnter), Some(0xE01C));
        assert_eq!(scancode(KeyCode::ControlLeft), Some(sc::LEFT_CTRL));
        assert_eq!(scancode(KeyCode::ControlRight), Some(sc::RIGHT_CTRL));
        assert_eq!(scancode(KeyCode::F12), Some(sc::F12));
        assert_eq!(scancode(KeyCode::ArrowLeft), Some(0xE04B));
        assert_eq!(scancode(KeyCode::SuperLeft), Some(sc::LEFT_META));
        assert_eq!(scancode(KeyCode::Pause), None);
        assert_eq!(scancode(KeyCode::MediaPlayPause), None);
    }

    #[test]
    fn modifiers_and_navigation_are_extended_where_windows_expects_it() {
        for key in [
            KeyCode::ControlRight,
            KeyCode::AltRight,
            KeyCode::ArrowUp,
            KeyCode::Delete,
            KeyCode::Home,
            KeyCode::SuperRight,
        ] {
            assert!(sc::is_extended(scancode(key).unwrap()), "{key:?}");
        }
        for key in [
            KeyCode::ControlLeft,
            KeyCode::AltLeft,
            KeyCode::ShiftRight,
            KeyCode::Numpad8,
        ] {
            assert!(!sc::is_extended(scancode(key).unwrap()), "{key:?}");
        }
    }
}
