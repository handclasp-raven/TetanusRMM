//! A company's own branding on what its users see: a name, a logo and an
//! accent colour in place of TetanusRMM's (see the `brand` crate for what
//! each changes). One per server, set by an admin.
//!
//! It reaches an agent in [`crate::Message::Branding`] and its helper in
//! [`crate::ipc::IpcMessage::Branding`]; the quick assist client carries
//! it in the configuration appended to its download (see
//! [`crate::assist`]), since it shows its first window before it talks to
//! the server.

use serde::{Deserialize, Serialize};

/// Longest company name, in characters.
pub const MAX_NAME_CHARS: usize = 48;
/// Largest logo file, in bytes.
pub const MAX_LOGO_BYTES: usize = 128 * 1024;
/// A logo's sides, in pixels, at least and at most.
pub const MIN_LOGO_PX: u32 = 16;
pub const MAX_LOGO_PX: u32 = 512;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branding {
    /// The company's name, as its users know it.
    pub name: String,
    /// Red, green, blue. `None`: TetanusRMM's rust.
    pub accent: Option<[u8; 3]>,
    /// A PNG. `None`: TetanusRMM's mark.
    #[serde(with = "logo")]
    pub logo_png: Option<Vec<u8>>,
}

impl std::fmt::Debug for Branding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Branding")
            .field("name", &self.name)
            .field(
                "accent",
                &self.accent.map(|[r, g, b]| brand::Rgb(r, g, b).to_hex()),
            )
            .field("logo_bytes", &self.logo_png.as_ref().map(Vec::len))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrandingError {
    #[error("the name must be 1 to {MAX_NAME_CHARS} characters")]
    Name,
    #[error("the accent colour is too light: white text must be readable on it")]
    AccentTooLight,
    #[error("the logo must be a PNG image")]
    NotPng,
    #[error("the logo is {0} bytes; at most {MAX_LOGO_BYTES} are allowed")]
    LogoTooLarge(usize),
    #[error("the logo is {0}x{1} pixels; each side must be {MIN_LOGO_PX} to {MAX_LOGO_PX}")]
    LogoSize(u32, u32),
}

impl Branding {
    /// Whether this can be shown: checked where it is set (the server) and
    /// again where it is used (an agent does not take the server's word).
    pub fn validate(&self) -> Result<(), BrandingError> {
        let chars = self.name.chars().count();
        if self.name.trim() != self.name
            || !(1..=MAX_NAME_CHARS).contains(&chars)
            || self.name.chars().any(char::is_control)
        {
            return Err(BrandingError::Name);
        }
        if let Some(accent) = self.accent_rgb() {
            if !brand::theme::accent_ok(accent) {
                return Err(BrandingError::AccentTooLight);
            }
        }
        if let Some(png) = &self.logo_png {
            if png.len() > MAX_LOGO_BYTES {
                return Err(BrandingError::LogoTooLarge(png.len()));
            }
            let (w, h) = brand::ico::png_size(png).ok_or(BrandingError::NotPng)?;
            let ok = |side| (MIN_LOGO_PX..=MAX_LOGO_PX).contains(&side);
            if !ok(w) || !ok(h) {
                return Err(BrandingError::LogoSize(w, h));
            }
        }
        Ok(())
    }

    pub fn accent_rgb(&self) -> Option<brand::Rgb> {
        self.accent.map(|[r, g, b]| brand::Rgb(r, g, b))
    }
}

/// The logo as bytes in binary formats, and as base64 text in JSON (the
/// API, and the quick assist client's configuration).
mod logo {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(logo: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            logo.as_deref().map(super::base64::encode).serialize(s)
        } else {
            logo.serialize(s)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        if d.is_human_readable() {
            Option::<String>::deserialize(d)?
                .map(|text| super::base64::decode(&text))
                .transpose()
                .map_err(serde::de::Error::custom)
        } else {
            Option::<Vec<u8>>::deserialize(d)
        }
    }
}

/// Standard base64, with padding.
pub mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn decode(text: &str) -> Result<Vec<u8>, &'static str> {
        let text = text.trim_end_matches('=').as_bytes();
        if text.len() % 4 == 1 {
            return Err("not base64");
        }
        let mut out = Vec::with_capacity(text.len() * 3 / 4);
        for chunk in text.chunks(4) {
            let mut n = 0u32;
            for (i, c) in chunk.iter().enumerate() {
                let value = ALPHABET.iter().position(|a| a == c).ok_or("not base64")?;
                n |= (value as u32) << (18 - 6 * i);
            }
            for i in 0..chunk.len() - 1 {
                out.push((n >> (16 - 8 * i)) as u8);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG header saying `width` x `height`, padded to `len` bytes.
    pub fn png(width: u32, height: u32, len: usize) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.resize(len.max(out.len()), 0);
        out
    }

    fn contoso() -> Branding {
        Branding {
            name: "Contoso IT".into(),
            accent: Some([0x0B, 0x5C, 0xAD]),
            logo_png: Some(png(128, 128, 200)),
        }
    }

    #[test]
    fn a_sound_branding_is_accepted_with_or_without_its_parts() {
        assert_eq!(contoso().validate(), Ok(()));
        let plain = Branding {
            name: "Contoso".into(),
            accent: None,
            logo_png: None,
        };
        assert_eq!(plain.validate(), Ok(()));
    }

    #[test]
    fn what_cannot_be_shown_is_refused() {
        let with = |change: fn(&mut Branding)| {
            let mut b = contoso();
            change(&mut b);
            b.validate()
        };
        assert_eq!(with(|b| b.name.clear()), Err(BrandingError::Name));
        assert_eq!(
            with(|b| b.name = " padded ".into()),
            Err(BrandingError::Name)
        );
        assert_eq!(with(|b| b.name = "x".repeat(49)), Err(BrandingError::Name));
        assert_eq!(with(|b| b.name = "a\nb".into()), Err(BrandingError::Name));
        assert_eq!(
            with(|b| b.accent = Some([0xFF, 0xD4, 0x00])),
            Err(BrandingError::AccentTooLight)
        );
        assert_eq!(
            with(|b| b.logo_png = Some(b"GIF89a".to_vec())),
            Err(BrandingError::NotPng)
        );
        assert_eq!(
            with(|b| b.logo_png = Some(png(64, 64, MAX_LOGO_BYTES + 1))),
            Err(BrandingError::LogoTooLarge(MAX_LOGO_BYTES + 1))
        );
        assert_eq!(
            with(|b| b.logo_png = Some(png(1024, 64, 100))),
            Err(BrandingError::LogoSize(1024, 64))
        );
        assert_eq!(
            with(|b| b.logo_png = Some(png(8, 8, 100))),
            Err(BrandingError::LogoSize(8, 8))
        );
    }

    #[test]
    fn the_logo_is_bytes_on_the_wire_and_base64_in_json() {
        let b = contoso();
        let wire = postcard::to_stdvec(&b).unwrap();
        assert!(wire.len() < 260, "{}", wire.len());
        assert_eq!(postcard::from_bytes::<Branding>(&wire).unwrap(), b);
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["name"], "Contoso IT");
        assert_eq!(json["accent"], serde_json::json!([11, 92, 173]));
        assert!(json["logo_png"]
            .as_str()
            .unwrap()
            .starts_with("iVBORw0KGgo"));
        assert_eq!(serde_json::from_value::<Branding>(json).unwrap(), b);
        let none = r#"{"name":"Contoso","accent":null,"logo_png":null}"#;
        assert_eq!(
            serde_json::from_str::<Branding>(none).unwrap().logo_png,
            None
        );
        assert!(
            serde_json::from_str::<Branding>(r#"{"name":"x","accent":null,"logo_png":"***"}"#)
                .is_err()
        );
    }

    #[test]
    fn base64_round_trips_every_length() {
        assert_eq!(base64::encode(b""), "");
        assert_eq!(base64::encode(b"f"), "Zg==");
        assert_eq!(base64::encode(b"fo"), "Zm8=");
        assert_eq!(base64::encode(b"foobar"), "Zm9vYmFy");
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base64::decode(&base64::encode(&bytes)).unwrap(), bytes);
        }
        assert!(base64::decode("Zg=x").is_err() && base64::decode("Z").is_err());
    }

    #[test]
    fn the_logo_never_shows_up_in_logs() {
        let shown = format!("{:?}", contoso());
        assert!(shown.contains("Contoso IT") && shown.contains("#0B5CAD"));
        assert!(shown.contains("logo_bytes: Some(200)") && !shown.contains("137"));
    }
}
