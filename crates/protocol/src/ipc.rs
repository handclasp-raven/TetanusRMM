//! Messages on the local named pipe between the agent service (session 0,
//! SYSTEM) and its helper process in the user's session.
//!
//! Framed with [`crate::framing`] like the network protocol. In this phase
//! the pipe only carries status for the tray icon; later phases add screen
//! capture frames (helper to service) and input events (service to helper).

use serde::{Deserialize, Serialize};

use crate::media::MonitorInfo;

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
    /// Service to helper: report the monitors.
    ListMonitors,
    /// Helper to service: the monitors (reply to `ListMonitors`, or on change).
    Monitors(Vec<MonitorInfo>),
    /// Service to helper: capture and encode `monitor`.
    StartCapture { monitor: u32 },
    /// Service to helper: stop capturing.
    StopCapture,
    /// Service to helper: next frame must be a keyframe.
    ForceKeyframe,
    /// Helper to service: one encoded frame (Annex B, keyframes carry SPS/PPS).
    Frame(EncodedFrame),
}

/// An encoded video frame from the helper.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedFrame {
    pub monitor: u32,
    pub keyframe: bool,
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
    pub h264: Vec<u8>,
}

impl std::fmt::Debug for EncodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedFrame")
            .field("monitor", &self.monitor)
            .field("keyframe", &self.keyframe)
            .field("pts_us", &self.pts_us)
            .field("size", &format_args!("{}x{}", self.width, self.height))
            .field("bytes", &self.h264.len())
            .finish()
    }
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
