//! Starting a program on the signed-in user's desktop, from the viewer's
//! command buttons (`cmd`, `ncpa.cpl`, `mstsc`, ...).
//!
//! The server opens a stream with [`crate::StreamOpen::Launch`]; the agent
//! starts the command in the console session **as the signed-in user**
//! (never as SYSTEM), on their desktop, so it shows up in the remote
//! desktop session, and answers with one [`LaunchReply`]. It does not wait
//! for the program to exit or collect its output: that is what scripts
//! are for.

use serde::{Deserialize, Serialize};

/// Longest command accepted, in bytes.
pub const MAX_COMMAND_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRequest {
    /// What to start, as typed at a Run prompt: a program (`cmd`), a
    /// control panel applet (`ncpa.cpl`), a console (`services.msc`),
    /// with arguments if needed (`mstsc /v:server01`).
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCommand {
    #[error("the command is empty")]
    Empty,
    #[error("the command is longer than {MAX_COMMAND_LEN} bytes")]
    TooLong,
    #[error("the command must be a single line")]
    ControlCharacters,
}

impl LaunchRequest {
    /// Trimmed, non-empty, one line, at most [`MAX_COMMAND_LEN`] bytes.
    pub fn new(command: &str) -> Result<Self, InvalidCommand> {
        let command = command.trim();
        if command.is_empty() {
            return Err(InvalidCommand::Empty);
        }
        if command.len() > MAX_COMMAND_LEN {
            return Err(InvalidCommand::TooLong);
        }
        if command.chars().any(char::is_control) {
            return Err(InvalidCommand::ControlCharacters);
        }
        Ok(Self {
            command: command.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchReply {
    /// Started; `user` is who it runs as (e.g. `CORP\alice`).
    Started { user: String },
    /// Nobody is signed in at the console to start it for.
    NoUser,
    /// Could not be started.
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StreamOpen;

    #[test]
    fn commands_are_validated() {
        assert_eq!(
            LaunchRequest::new("  ncpa.cpl ").unwrap().command,
            "ncpa.cpl"
        );
        assert_eq!(LaunchRequest::new(" \t"), Err(InvalidCommand::Empty));
        assert_eq!(
            LaunchRequest::new(&"x".repeat(MAX_COMMAND_LEN + 1)),
            Err(InvalidCommand::TooLong)
        );
        assert_eq!(
            LaunchRequest::new("cmd\r\ncalc"),
            Err(InvalidCommand::ControlCharacters)
        );
    }

    #[test]
    fn launch_round_trips() {
        let open = StreamOpen::Launch(LaunchRequest::new("mstsc /v:srv01").unwrap());
        let bytes = postcard::to_stdvec(&open).unwrap();
        assert_eq!(postcard::from_bytes::<StreamOpen>(&bytes).unwrap(), open);
        let reply = LaunchReply::Started {
            user: "CORP\\alice".into(),
        };
        let bytes = postcard::to_stdvec(&reply).unwrap();
        assert_eq!(postcard::from_bytes::<LaunchReply>(&bytes).unwrap(), reply);
    }
}
