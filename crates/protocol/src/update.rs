//! Agent auto-update manifest and the exact bytes that get signed.
//!
//! The ed25519 signature does not cover the binary alone. It covers
//! [`signed_message`], which binds the binary's SHA-256 to its platform and
//! version. So a correctly signed old binary cannot be replayed as a "newer"
//! version (downgrade), and a Linux build cannot be served to Windows.

use serde::{Deserialize, Serialize};

/// Published at `GET /api/updates/{platform}/manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub platform: String,
    /// Semantic version, e.g. `0.2.0`.
    pub version: String,
    /// Lowercase hex SHA-256 of the binary.
    pub sha256: String,
    pub size: u64,
}

const DOMAIN: &str = "rmm-agent-update-v1";

/// The message the update signing key signs for a release.
///
/// Each field is length-prefixed so no choice of field contents can make two
/// different releases produce the same bytes.
pub fn signed_message(platform: &str, version: &str, sha256_hex: &str) -> Vec<u8> {
    let mut out = format!("{DOMAIN}\n").into_bytes();
    for field in [platform, version, sha256_hex] {
        out.extend_from_slice(format!("{}:", field.len()).as_bytes());
        out.extend_from_slice(field.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Platform identifier of the running build, e.g. `windows-x86_64`.
pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Whether `platform` is safe to use as a single path segment.
pub fn valid_platform(platform: &str) -> bool {
    !platform.is_empty()
        && platform.len() <= 64
        && platform
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_message_binds_every_field() {
        let base = signed_message("windows-x86_64", "1.2.3", "ab");
        assert_ne!(base, signed_message("linux-x86_64", "1.2.3", "ab"));
        assert_ne!(base, signed_message("windows-x86_64", "1.2.4", "ab"));
        assert_ne!(base, signed_message("windows-x86_64", "1.2.3", "ac"));
        // Fields cannot bleed into each other.
        assert_ne!(
            signed_message("a\nb", "c", "d"),
            signed_message("a", "b\nc", "d")
        );
    }

    #[test]
    fn platform_names_are_path_safe() {
        assert!(valid_platform(&current_platform()));
        assert!(valid_platform("windows-x86_64"));
        for bad in ["", "../etc", "a/b", "a b", "a\\b"] {
            assert!(!valid_platform(bad), "{bad:?}");
        }
    }
}
