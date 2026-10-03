//! A password the user lends to the technicians for a remote session.
//!
//! A technician asks for it from the viewer; the user types it into a
//! prompt on their own desktop. The agent keeps it, encrypted in memory,
//! on the user's machine and nowhere else: it is never sent to a viewer or
//! to the server. A technician can have the agent type it (at the lock
//! screen, a UAC prompt, a login form) for as long as any remote session
//! is open. When the last one ends, or the user presses Ctrl+F12, it is
//! forgotten.
//!
//! What does travel is a [`CredentialEvent`]: to the technician who asked,
//! so the viewer can say what happened, and to the server for the audit
//! log.

use serde::{Deserialize, Serialize};

/// Something that happened to the lent password. Never the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialEvent {
    /// The user is being asked.
    Requested,
    /// The user typed one; the agent holds it now.
    Stored,
    /// The user said no, or did not answer in time.
    Declined,
    /// It cannot be asked for or kept here: nobody is at the desktop, or
    /// the server cannot audit it.
    Unavailable,
    /// The agent typed it for a technician.
    Typed,
    /// The agent no longer holds it: a technician said to forget it, or
    /// the last session ended.
    Forgotten,
    /// A technician wanted it typed, but the agent holds none.
    NotStored,
}

impl CredentialEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialEvent::Requested => "requested",
            CredentialEvent::Stored => "stored",
            CredentialEvent::Declined => "declined",
            CredentialEvent::Unavailable => "unavailable",
            CredentialEvent::Typed => "typed",
            CredentialEvent::Forgotten => "forgotten",
            CredentialEvent::NotStored => "not_stored",
        }
    }
}

impl std::fmt::Display for CredentialEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Longest password the prompt takes, in UTF-16 units (Windows' own limit
/// for an account password is 256).
pub const MAX_PASSWORD_UNITS: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialise_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&CredentialEvent::NotStored).unwrap(),
            "\"not_stored\""
        );
        assert_eq!(CredentialEvent::Stored.to_string(), "stored");
    }
}
