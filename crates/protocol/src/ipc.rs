//! Messages on the local named pipe between the agent service (session 0,
//! SYSTEM) and its helper process in the user's session.
//!
//! Framed with [`crate::framing`] like the network protocol. In this phase
//! the pipe only carries status for the tray icon; later phases add screen
//! capture frames (helper to service) and input events (service to helper).

use serde::{Deserialize, Serialize};

/// Well-known pipe name. Created by the service with `first_pipe_instance`,
/// so another process cannot squat on it first.
pub const PIPE_NAME: &str = r"\\.\pipe\rmm-agent-helper";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcMessage {
    /// Helper to service, first message after connecting.
    HelperHello {
        pid: u32,
        session_id: u32,
        version: String,
    },
    /// Service to helper: what the tray should show. Sent on connect and
    /// whenever it changes.
    Status(AgentStatus),
}

/// Agent state as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    /// `None` until enrolled.
    pub agent_id: Option<String>,
    /// Whether the control connection to the server is up.
    pub connected: bool,
    pub version: String,
}

impl AgentStatus {
    /// One-line summary for the tray tooltip.
    pub fn tooltip(&self) -> String {
        let state = match (&self.agent_id, self.connected) {
            (None, _) => "not enrolled".to_owned(),
            (Some(_), true) => "connected".to_owned(),
            (Some(_), false) => "disconnected".to_owned(),
        };
        format!("RMM Agent {} - {state}", self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{read_frame, write_frame};

    #[tokio::test]
    async fn ipc_messages_round_trip_through_framing() {
        let msgs = vec![
            IpcMessage::HelperHello {
                pid: 4242,
                session_id: 1,
                version: "0.2.0".into(),
            },
            IpcMessage::Status(AgentStatus {
                agent_id: Some("agt-1".into()),
                connected: true,
                version: "0.2.0".into(),
            }),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_frame(&mut buf, m).await.unwrap();
        }
        let mut reader = buf.as_slice();
        for m in msgs {
            let got: Option<IpcMessage> = read_frame(&mut reader).await.unwrap();
            assert_eq!(got, Some(m));
        }
    }

    #[test]
    fn tooltip_describes_state() {
        let mut s = AgentStatus {
            agent_id: None,
            connected: false,
            version: "1.0.0".into(),
        };
        assert_eq!(s.tooltip(), "RMM Agent 1.0.0 - not enrolled");
        s.agent_id = Some("agt-1".into());
        assert_eq!(s.tooltip(), "RMM Agent 1.0.0 - disconnected");
        s.connected = true;
        assert_eq!(s.tooltip(), "RMM Agent 1.0.0 - connected");
    }
}
