//! Colours, and the arithmetic the themes need: mixing, and the contrast
//! between a text colour and what it sits on.

/// An opaque sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const WHITE: Rgb = Rgb(0xFF, 0xFF, 0xFF);
    pub const BLACK: Rgb = Rgb(0, 0, 0);

    /// From `0xRRGGBB`.
    pub const fn hex(value: u32) -> Self {
        Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    /// From `#RRGGBB` (the `#` is optional).
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(digits, 16).ok().map(Self::hex)
    }

    /// As `#RRGGBB`.
    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }

    pub fn to_array(self) -> [u8; 3] {
        [self.0, self.1, self.2]
    }

    /// `self` with `amount` (0-1) of `other` mixed in.
    pub fn mix(self, other: Rgb, amount: f32) -> Rgb {
        let t = amount.clamp(0.0, 1.0);
        let ch = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(
            ch(self.0, other.0),
            ch(self.1, other.1),
            ch(self.2, other.2),
        )
    }

    /// Relative luminance (WCAG): 0 for black, 1 for white.
    pub fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    /// Contrast ratio with `other` (WCAG): 1 for the same colour, 21 for
    /// black on white.
    pub fn contrast(self, other: Rgb) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// `self` moved towards `towards` until it stands out from `on` by at
    /// least `ratio` (or as far as it goes).
    pub fn readable_on(self, on: Rgb, ratio: f32, towards: Rgb) -> Rgb {
        (0..=20)
            .map(|step| self.mix(towards, step as f32 / 20.0))
            .find(|c| c.contrast(on) >= ratio)
            .unwrap_or(towards)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let rust = Rgb::hex(0xB5441C);
        assert_eq!(rust, Rgb(0xB5, 0x44, 0x1C));
        assert_eq!(rust.to_hex(), "#B5441C");
        assert_eq!(Rgb::parse("#b5441c"), Some(rust));
        assert_eq!(Rgb::parse("B5441C"), Some(rust));
        for bad in ["", "#B5441", "#B5441CC", "#GGGGGG", "rust"] {
            assert_eq!(Rgb::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn contrast_matches_the_standard() {
        assert!((Rgb::WHITE.contrast(Rgb::BLACK) - 21.0).abs() < 0.01);
        assert!((Rgb::WHITE.contrast(Rgb::WHITE) - 1.0).abs() < 0.001);
        // White on Rust, the primary button: comfortably readable.
        let ratio = Rgb::WHITE.contrast(Rgb::hex(0xB5441C));
        assert!((5.3..5.8).contains(&ratio), "{ratio}");
    }

    #[test]
    fn mixing_and_making_readable() {
        assert_eq!(Rgb::BLACK.mix(Rgb::WHITE, 0.0), Rgb::BLACK);
        assert_eq!(Rgb::BLACK.mix(Rgb::WHITE, 1.0), Rgb::WHITE);
        assert_eq!(Rgb::BLACK.mix(Rgb::WHITE, 0.5), Rgb(128, 128, 128));
        let dark = Rgb::hex(0x1E2328);
        let navy = Rgb::hex(0x001F5B);
        let lifted = navy.readable_on(dark, 4.5, Rgb::WHITE);
        assert!(lifted.contrast(dark) >= 4.5 && lifted != navy);
        // Already readable: left alone.
        assert_eq!(Rgb::WHITE.readable_on(dark, 4.5, Rgb::WHITE), Rgb::WHITE);
    }
}
