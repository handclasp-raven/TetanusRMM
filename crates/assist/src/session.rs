//! A quick assist session, apart from its window: trade the code for a
//! short-lived credential, then be an agent until the program is closed.
//!
//! The credential is kept in memory only. Nothing is installed and nothing
//! is stored: when the program exits, the server deletes the throwaway
//! agent a couple of minutes later.

use std::sync::Arc;
use std::time::Duration;

use agent::core::{self, CoreOptions};
use agent::credstore::Credential;
use agent::enroll::EnrollOptions;
use agent::interactive::DesktopLink;
use agent::media::source::MediaLink;
use agent::AgentError;
use protocol::assist::{AssistConfig, REJECT_CODE, REJECT_TOO_MANY};
use protocol::ipc::AgentStatus;
use tokio::sync::watch;

/// The user's screen and desktop, if this build can reach them.
#[derive(Clone, Default)]
pub struct Links {
    pub media: Option<Arc<MediaLink>>,
    pub desktop: Option<Arc<DesktopLink>>,
}

/// Why a code did not start a session, in words for the user.
pub fn failure_text(error: &AgentError) -> String {
    match error {
        AgentError::Enroll(reason) if reason == REJECT_CODE => {
            "That code was not recognised. Check it with the person supporting you. \
             Codes work once and expire after ten minutes."
                .to_owned()
        }
        AgentError::Enroll(reason) if reason == REJECT_TOO_MANY => {
            "Too many wrong codes. Wait ten minutes, then ask for a new code.".to_owned()
        }
        AgentError::Unreachable(_) | AgentError::Connection(_) => {
            "Cannot reach the support server. Check your internet connection and try again."
                .to_owned()
        }
        other => format!("Could not start the session: {other}"),
    }
}

/// Trade `code` for a credential.
pub async fn redeem(config: &AssistConfig, code: &str) -> Result<Credential, AgentError> {
    let server = config.server.clone();
    // Resolved now, not when the file was made: the user may be anywhere.
    let server_addr = tokio::task::spawn_blocking(move || agent::resolve_server(&server))
        .await
        .map_err(|e| AgentError::Unreachable(e.to_string()))?
        .map_err(AgentError::Unreachable)?;
    agent::enroll::enroll_assist(&EnrollOptions {
        server_addr,
        transport: transport::TransportSettings::default(),
        server_name: config.server_name.clone(),
        server_ca_pem: config.ca_pem.clone(),
        token: code.to_owned(),
        bind_addr: None,
    })
    .await
}

/// Be the session's agent, reconnecting as needed, until dropped. Only
/// returns if it cannot start at all.
pub async fn serve(
    credential: &Credential,
    links: Links,
    status: watch::Sender<AgentStatus>,
) -> anyhow::Error {
    let options = CoreOptions {
        heartbeat_interval: agent::DEFAULT_HEARTBEAT_INTERVAL,
        // It lives for one session: there is nothing to update.
        update_interval: Duration::ZERO,
        store: None,
        telemetry: None,
        media: links.media,
        desktop: links.desktop,
        remote: agent::remote::Allowed::FilesOnly,
    };
    match core::run(credential, options, status).await {
        Ok(_) => anyhow::anyhow!("the session stopped unexpectedly"),
        Err(e) => e,
    }
}

/// Where the window is in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for a code to be typed.
    #[cfg_attr(not(windows), allow(dead_code))]
    Idle,
    /// The code is with the server.
    Checking,
    /// The code was accepted.
    Session,
}

/// The status line of the console build. (The Windows window says the same
/// in its own pages.)
#[cfg_attr(windows, allow(dead_code))]
pub fn status_text(phase: Phase, connected: bool, technicians: &[String]) -> String {
    match phase {
        Phase::Idle => String::new(),
        Phase::Checking => "Checking the code\u{2026}".to_owned(),
        Phase::Session if !connected => "Connecting to the support server\u{2026}".to_owned(),
        Phase::Session if technicians.is_empty() => {
            "Ready. Waiting for your supporter. You will be asked before they can see \
             your screen."
                .to_owned()
        }
        Phase::Session => {
            let mut names: Vec<&str> = Vec::new();
            for name in technicians {
                if !names.contains(&name.as_str()) {
                    names.push(name);
                }
            }
            format!(
                "{} can see and control this computer. Press Ctrl+F12 to stop them, or \
                 close this window to end the session.",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejections_are_explained_in_plain_words() {
        let wrong = failure_text(&AgentError::Enroll(REJECT_CODE.into()));
        assert!(wrong.contains("not recognised"));
        let limited = failure_text(&AgentError::Enroll(REJECT_TOO_MANY.into()));
        assert!(limited.contains("Too many"));
        let offline = failure_text(&AgentError::Unreachable("timed out".into()));
        assert!(offline.contains("internet connection"));
        assert!(failure_text(&AgentError::StreamClosed).contains("Could not start"));
    }

    #[test]
    fn the_status_line_says_who_is_watching() {
        assert_eq!(status_text(Phase::Idle, false, &[]), "");
        assert!(status_text(Phase::Checking, false, &[]).starts_with("Checking"));
        assert!(status_text(Phase::Session, false, &[]).starts_with("Connecting"));
        assert!(status_text(Phase::Session, true, &[]).starts_with("Ready"));
        let names = vec!["jane".to_owned(), "jane".to_owned(), "sam".to_owned()];
        let watching = status_text(Phase::Session, true, &names);
        assert!(watching.starts_with("jane, sam can see and control"));
        assert!(watching.contains("Ctrl+F12"));
    }
}
