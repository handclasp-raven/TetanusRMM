//! Quick assist: one-time support sessions on machines with no agent
//! installed.
//!
//! The user downloads a single executable from the server and types a
//! six-digit code a technician gave them. The executable is the same for
//! every server; what differs is appended to it when it is downloaded: an
//! [`AssistConfig`] saying where the server is and which CA to trust.
//!
//! Layout of a downloaded file:
//!
//! ```text
//! executable | config (JSON) | config length (u32, big-endian) | MAGIC
//! ```
//!
//! Windows ignores bytes after the last section of an executable, so the
//! program runs as built and reads the tail of its own file.

use serde::{Deserialize, Serialize};

/// Digits in a quick assist code.
pub const CODE_LEN: usize = 6;

/// Last bytes of a file that carries a config.
pub const MAGIC: &[u8; 8] = b"RMMASST1";

/// Why the server closed a connection that sent a wrong (or expired, or
/// used) code.
pub const REJECT_CODE: &str = "invalid quick assist code";
/// Why the server closed a connection from an address that has sent too
/// many wrong codes.
pub const REJECT_TOO_MANY: &str = "too many wrong quick assist codes; try again later";

/// Largest config accepted (a CA certificate is a few kilobytes).
pub const MAX_CONFIG_LEN: usize = 64 * 1024;

/// Where a quick assist client connects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistConfig {
    /// QUIC address, `host:port` (resolved when connecting).
    pub server: String,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    /// PEM CA the server certificate must chain to.
    pub ca_pem: String,
}

/// The bytes to append to the executable for `config`.
pub fn trailer(config: &AssistConfig) -> Vec<u8> {
    let mut out = serde_json::to_vec(config).expect("config serializes");
    let len = u32::try_from(out.len()).expect("config is small");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(MAGIC);
    out
}

/// The config at the end of `file`, if it carries a well-formed one.
pub fn read_trailer(file: &[u8]) -> Option<AssistConfig> {
    let rest = file.strip_suffix(MAGIC)?;
    let (rest, len) = rest.split_last_chunk::<4>()?;
    let len = u32::from_be_bytes(*len) as usize;
    if len > MAX_CONFIG_LEN || len > rest.len() {
        return None;
    }
    serde_json::from_slice(&rest[rest.len() - len..]).ok()
}

/// `code` as typed, reduced to its digits: `Some` if exactly [`CODE_LEN`]
/// remain. Spaces and dashes are how people read codes out, so they are
/// dropped; anything else makes it not a code.
pub fn normalize_code(code: &str) -> Option<String> {
    let mut digits = String::with_capacity(CODE_LEN);
    for c in code.chars() {
        match c {
            '0'..='9' => digits.push(c),
            ' ' | '-' => {}
            _ => return None,
        }
    }
    (digits.len() == CODE_LEN).then_some(digits)
}

/// Seconds the scam warning must be on screen before it can be accepted.
pub const WARNING_DELAY_SECS: u32 = 5;

/// What the user must accept before a quick assist client will take a code.
pub const SCAM_WARNING: &str = "Only continue if you know and trust the person supporting you.\n\n\
     A real business will never ask you to pay for anything with gift cards or \
     cryptocurrency. Only scammers do this.\n\n\
     If someone contacted you unexpectedly, close this window now.";

/// The accept button's label and whether it can be pressed, with
/// `remaining` seconds of the delay left.
pub fn accept_button(remaining: u32) -> (String, bool) {
    if remaining == 0 {
        ("I understand".to_owned(), true)
    } else {
        (format!("I understand ({remaining})"), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AssistConfig {
        AssistConfig {
            server: "rmm.example.com:4433".into(),
            server_name: "rmm.example.com".into(),
            ca_pem: "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----\n".into(),
        }
    }

    #[test]
    fn a_config_appended_to_an_executable_is_read_back() {
        let mut file = b"MZ pretend executable".to_vec();
        assert_eq!(read_trailer(&file), None);
        file.extend(trailer(&config()));
        assert_eq!(read_trailer(&file), Some(config()));
    }

    #[test]
    fn damaged_trailers_are_not_configs() {
        let good = trailer(&config());
        // Cut short, a length beyond the file, and JSON that is not a config.
        assert_eq!(read_trailer(&good[1..]), None);
        let mut long = good.clone();
        let at = long.len() - MAGIC.len() - 4;
        long[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(read_trailer(&long), None);
        let mut other = b"{}".to_vec();
        other.extend_from_slice(&2u32.to_be_bytes());
        other.extend_from_slice(MAGIC);
        assert_eq!(read_trailer(&other), None);
        assert_eq!(read_trailer(MAGIC), None);
    }

    #[test]
    fn codes_are_six_digits_however_they_are_spaced() {
        assert_eq!(normalize_code("482913").as_deref(), Some("482913"));
        assert_eq!(normalize_code(" 482 913 ").as_deref(), Some("482913"));
        assert_eq!(normalize_code("482-913").as_deref(), Some("482913"));
        assert_eq!(normalize_code("48291"), None);
        assert_eq!(normalize_code("4829130"), None);
        assert_eq!(normalize_code("48291a"), None);
        assert_eq!(normalize_code(""), None);
    }

    #[test]
    fn the_warning_cannot_be_accepted_until_the_delay_has_passed() {
        for remaining in 1..=WARNING_DELAY_SECS {
            let (label, enabled) = accept_button(remaining);
            assert!(!enabled);
            assert_eq!(label, format!("I understand ({remaining})"));
        }
        assert_eq!(accept_button(0), ("I understand".to_owned(), true));
        assert!(SCAM_WARNING.contains("gift cards") && SCAM_WARNING.contains("cryptocurrency"));
    }
}
