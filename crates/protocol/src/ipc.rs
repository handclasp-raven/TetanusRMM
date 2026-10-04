//! Messages on the local named pipe between the agent service (session 0,
//! SYSTEM) and its helper processes in the console session.
//!
//! Framed with [`crate::framing`] like the network protocol. It carries the
//! tray status, capture control and frames, and (Phase 6) the consent
//! prompt, toasts, the technician list, input, clipboard and the Ctrl+F12
//! kill switch. It also carries the password a user lends the technicians
//! (see [`crate::credential`]), as a [`Secret`]: from the prompt to the
//! service, which keeps it, and from the service to the helper that types
//! it.

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::brand::Branding;
use crate::clipboard::ClipboardData;
use crate::consent::PromptAnswer;
use crate::input::InputEvent;
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
    /// Service to helper: show the blocking consent prompt.
    ConsentPrompt {
        request_id: u64,
        technician: String,
        timeout_secs: u32,
    },
    /// Service to helper: the request was withdrawn; close its prompt.
    ConsentCancel { request_id: u64 },
    /// Helper to service: how the prompt was answered.
    ConsentAnswer {
        request_id: u64,
        answer: PromptAnswer,
    },
    /// Service to helper: show a toast naming the technician who connected.
    Toast { technician: String },
    /// Service to helper: everyone currently connected, for the tray.
    Technicians(Vec<String>),
    /// Service to helper: inject input.
    Input(InputEvent),
    /// Service to helper: put this on the user's clipboard.
    SetClipboard(ClipboardData),
    /// Helper to service: the user's clipboard changed.
    Clipboard(ClipboardData),
    /// Helper to service: the user pressed Ctrl+F12.
    KillSwitch,
    /// Service to helper: encode at (at most) this many bits per second,
    /// from now on and for streams started later (adaptive bitrate).
    SetBitrate { bps: u32 },
    /// Service to helper: capture at most this many frames a second, from
    /// now on and for streams started later.
    SetFrameRate { fps: u32 },
    /// System helper to service: which kind of desktop is receiving input
    /// in its session. `secure` is the logon screen, the lock screen or a
    /// UAC prompt, which only the system helper can capture.
    Desktop { secure: bool },
    /// Service to helper: ask the user for a password to lend to the
    /// technicians, for `technician`.
    CredentialPrompt { request_id: u64, technician: String },
    /// Service to helper: the request was withdrawn; close its prompt.
    CredentialCancel { request_id: u64 },
    /// Helper to service: what the user typed, or `None` if they declined
    /// or did not answer.
    CredentialAnswer {
        request_id: u64,
        secret: Option<Secret>,
    },
    /// Service to helper: type this where the keyboard focus is.
    TypeText(Secret),
    /// Service to helper: the company's branding for everything the helper
    /// shows (see [`crate::brand`]), or `None` for TetanusRMM's own. Sent
    /// when the server has said, and again when it changes.
    Branding(Option<Branding>),
}

/// Text that must not be seen: a password, as UTF-16 units (what Windows'
/// edit controls give and `SendInput` takes). It is wiped when dropped
/// and never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret(Vec<u16>);

impl Secret {
    pub fn new(units: Vec<u16>) -> Self {
        Self(units)
    }

    pub fn units(&self) -> &[u16] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
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
    /// What the agent's connection is doing, in a word or two.
    pub fn state(&self) -> &'static str {
        match (&self.agent_id, self.connected) {
            (None, _) => "Not enrolled",
            (Some(_), true) => "Connected",
            (Some(_), false) => "Disconnected",
        }
    }

    /// One-line summary for the tray tooltip. `name` is what the agent is
    /// called (the product's name, or a company's).
    pub fn tooltip(&self, name: &str) -> String {
        format!("{name} {} - {}", self.version, self.state().to_lowercase())
    }

    /// The tray tooltip, naming everyone connected. Windows truncates
    /// tooltips at [`TOOLTIP_MAX`] characters, so a long list is cut with
    /// "+N more" rather than mid-name.
    pub fn tooltip_with(&self, name: &str, technicians: &[String]) -> String {
        let base = self.tooltip(name);
        if technicians.is_empty() {
            return base;
        }
        let head = format!("{base}\nConnected: ");
        for shown in (1..=technicians.len()).rev() {
            let mut text = head.clone() + &technicians[..shown].join(", ");
            let hidden = technicians.len() - shown;
            if hidden > 0 {
                text += &format!(" +{hidden} more");
            }
            if text.chars().count() <= TOOLTIP_MAX {
                return text;
            }
        }
        format!("{head}{} technicians", technicians.len())
    }
}

/// Longest tray tooltip Windows shows (`NOTIFYICONDATAW::szTip` is 128
/// UTF-16 units including the terminator).
pub const TOOLTIP_MAX: usize = 127;

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
            IpcMessage::ConsentPrompt {
                request_id: 9,
                technician: "Jane Doe".into(),
                timeout_secs: 30,
            },
            IpcMessage::ConsentAnswer {
                request_id: 9,
                answer: PromptAnswer::TimedOut,
            },
            IpcMessage::ConsentCancel { request_id: 9 },
            IpcMessage::Toast {
                technician: "Jane Doe".into(),
            },
            IpcMessage::Technicians(vec!["Jane Doe".into(), "Sam".into()]),
            IpcMessage::Input(InputEvent::Key {
                scancode: 0x1E,
                down: true,
            }),
            IpcMessage::SetClipboard(ClipboardData::Text("hi".into())),
            IpcMessage::Clipboard(ClipboardData::Files(vec![r"C:\a".into()])),
            IpcMessage::KillSwitch,
            IpcMessage::SetBitrate { bps: 1_500_000 },
            IpcMessage::SetFrameRate { fps: 60 },
            IpcMessage::Desktop { secure: true },
            IpcMessage::CredentialPrompt {
                request_id: 3,
                technician: "Jane Doe".into(),
            },
            IpcMessage::CredentialCancel { request_id: 3 },
            IpcMessage::CredentialAnswer {
                request_id: 3,
                secret: Some(Secret::new("hunter2".encode_utf16().collect())),
            },
            IpcMessage::CredentialAnswer {
                request_id: 3,
                secret: None,
            },
            IpcMessage::TypeText(Secret::new(vec![0x70, 0x77])),
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
    fn secrets_never_show_up_in_logs() {
        let secret = Secret::new("hunter2".encode_utf16().collect());
        let shown = format!(
            "{:?}",
            IpcMessage::CredentialAnswer {
                request_id: 1,
                secret: Some(secret.clone()),
            }
        );
        assert!(shown.contains("Secret(..)"), "{shown}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("104"),
            "{shown}"
        );
        assert!(!format!("{:?}", IpcMessage::TypeText(secret)).contains("104"));
    }

    const NAME: &str = "TetanusRMM Agent";

    #[test]
    fn tooltip_lists_technicians_within_the_windows_limit() {
        let s = AgentStatus {
            agent_id: Some("agt-1".into()),
            connected: true,
            version: "1.0.0".into(),
        };
        assert_eq!(
            s.tooltip_with(NAME, &[]),
            "TetanusRMM Agent 1.0.0 - connected"
        );
        let two = ["jane".to_owned(), "sam".to_owned()];
        assert_eq!(
            s.tooltip_with(NAME, &two),
            "TetanusRMM Agent 1.0.0 - connected\nConnected: jane, sam"
        );
        let many: Vec<String> = (0..20).map(|i| format!("technician-{i:02}")).collect();
        let tip = s.tooltip_with(NAME, &many);
        assert!(tip.chars().count() <= TOOLTIP_MAX, "{tip}");
        assert!(
            tip.contains("technician-00, ") && tip.ends_with(" more"),
            "{tip}"
        );
        let huge = vec!["x".repeat(200)];
        let tip = s.tooltip_with(NAME, &huge);
        assert!(tip.ends_with("1 technicians") && tip.chars().count() <= TOOLTIP_MAX);
    }

    #[test]
    fn tooltip_describes_state() {
        let mut s = AgentStatus {
            agent_id: None,
            connected: false,
            version: "1.0.0".into(),
        };
        assert_eq!(s.tooltip(NAME), "TetanusRMM Agent 1.0.0 - not enrolled");
        s.agent_id = Some("agt-1".into());
        assert_eq!(s.tooltip(NAME), "TetanusRMM Agent 1.0.0 - disconnected");
        s.connected = true;
        assert_eq!(s.tooltip(NAME), "TetanusRMM Agent 1.0.0 - connected");
    }
}
