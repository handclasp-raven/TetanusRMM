//! Text a technician has typed on the remote machine from their password
//! manager: a username, a password or a one-time code.
//!
//! The viewer looks it up in the technician's own vault and sends it to
//! the agent in a sealed record ([`crate::e2e::Control::TypeText`]), so
//! the server never sees it. The agent types it where the keyboard focus
//! is, as it does the lent password (see [`crate::credential`]), and
//! reports that it did ([`crate::Message::TextTyped`]) for the audit log:
//! which kind, and the name of the vault item, never the text.

use serde::{Deserialize, Serialize};

/// What the typed text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextKind {
    Username,
    Password,
    /// A time-based one-time code.
    Totp,
}

impl TextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TextKind::Username => "username",
            TextKind::Password => "password",
            TextKind::Totp => "totp",
        }
    }
}

impl std::fmt::Display for TextKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What came of a [`crate::e2e::Control::TypeText`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeTextResult {
    Typed,
    /// Nothing on the remote machine took the input.
    Blocked,
    /// The agent will not type it: the server cannot audit it, or it is
    /// too long.
    Unavailable,
}

/// Longest text the agent types, in UTF-16 units.
pub const MAX_TEXT_UNITS: usize = 1024;

/// Longest vault item name kept for the audit log, in characters.
pub const MAX_ITEM_CHARS: usize = 128;

/// `item` as the audit log keeps it: no control characters, at most
/// [`MAX_ITEM_CHARS`] long.
pub fn sanitize_item(item: &str) -> String {
    item.chars()
        .filter(|c| !c.is_control())
        .take(MAX_ITEM_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_serialise_in_snake_case() {
        assert_eq!(serde_json::to_string(&TextKind::Totp).unwrap(), "\"totp\"");
        assert_eq!(TextKind::Username.to_string(), "username");
    }

    #[test]
    fn item_names_are_cut_and_cleaned() {
        assert_eq!(sanitize_item(" Contoso\r\n admin\t"), "Contoso admin");
        assert_eq!(
            sanitize_item(&"x".repeat(500)).chars().count(),
            MAX_ITEM_CHARS
        );
    }
}
