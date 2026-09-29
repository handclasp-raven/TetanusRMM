//! Interactive remote shell (PowerShell on a ConPTY).
//!
//! Two legs, both carrying the same terminal bytes:
//!
//! ```text
//!  client ──WebSocket (HTTPS API)──▶ server ──QUIC bi stream──▶ agent (PTY)
//! ```
//!
//! - **Server ↔ agent:** a bidirectional stream opened by the server with
//!   [`crate::StreamOpen::Shell`]. After that the server writes
//!   [`ShellInput`] frames and the agent writes [`ShellOutput`] frames.
//!   The server finishing its side hangs up: the agent kills the shell.
//! - **Client ↔ server:** a WebSocket. Binary messages are raw terminal bytes
//!   in both directions (keystrokes in, VT-encoded output out). Text messages
//!   are JSON control messages: [`ClientControl`] from the client,
//!   [`ServerControl`] from the server. This keeps a Python client (the TUI)
//!   free of any binary encoding beyond plain bytes and JSON.

use serde::{Deserialize, Serialize};

/// Largest terminal dimension accepted, in cells.
pub const MAX_DIMENSION: u16 = 1000;

/// Largest `Data` payload in one frame. Bigger writes are split.
pub const MAX_DATA_CHUNK: usize = 64 * 1024;

/// Terminal size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

impl TermSize {
    /// A size, if both dimensions are within `1..=MAX_DIMENSION`.
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        let ok = |n: u16| (1..=MAX_DIMENSION).contains(&n);
        (ok(cols) && ok(rows)).then_some(Self { cols, rows })
    }

    pub fn is_valid(self) -> bool {
        Self::new(self.cols, self.rows).is_some()
    }
}

impl Default for TermSize {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 30,
        }
    }
}

/// Server to agent, after `StreamOpen::Shell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellInput {
    /// Bytes typed by the technician.
    Data(Vec<u8>),
    Resize(TermSize),
}

/// Agent to server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellOutput {
    /// The shell is running. Always the first frame on success.
    Started,
    /// Terminal output.
    Data(Vec<u8>),
    /// The shell exited; the agent finishes the stream after this.
    /// `code` is `None` if it could not be determined.
    Exited { code: Option<i32> },
    /// The shell could not be started, or failed. Last frame.
    Error(String),
}

/// JSON text message from the WebSocket client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientControl {
    Resize { cols: u16, rows: u16 },
}

/// JSON text message to the WebSocket client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerControl {
    /// Sent once, before any output.
    Started,
    /// The shell exited; the server closes the WebSocket next.
    Exit { code: Option<i32> },
    /// The shell failed; the server closes the WebSocket next.
    Error { message: String },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("invalid control message: {0}")]
    Malformed(String),
    #[error("terminal size must be 1-{MAX_DIMENSION} columns and rows")]
    BadSize,
}

/// Turn a client's JSON text message into input for the agent.
pub fn parse_client_control(text: &str) -> Result<ShellInput, ControlError> {
    match serde_json::from_str::<ClientControl>(text) {
        Ok(ClientControl::Resize { cols, rows }) => TermSize::new(cols, rows)
            .map(ShellInput::Resize)
            .ok_or(ControlError::BadSize),
        Err(e) => Err(ControlError::Malformed(e.to_string())),
    }
}

/// A message for the WebSocket client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    Binary(Vec<u8>),
    /// JSON-encoded [`ServerControl`].
    Text(String),
}

impl ShellOutput {
    /// How this agent frame is shown to the WebSocket client, and whether
    /// it is the last one.
    pub fn into_client_frame(self) -> (ClientFrame, bool) {
        let control = |c: ServerControl| {
            ClientFrame::Text(serde_json::to_string(&c).expect("control messages serialize"))
        };
        match self {
            ShellOutput::Data(bytes) => (ClientFrame::Binary(bytes), false),
            ShellOutput::Started => (control(ServerControl::Started), false),
            ShellOutput::Exited { code } => (control(ServerControl::Exit { code }), true),
            ShellOutput::Error(message) => (control(ServerControl::Error { message }), true),
        }
    }
}

/// Split terminal bytes into `Data` frames of at most [`MAX_DATA_CHUNK`].
pub fn data_frames(bytes: &[u8]) -> impl Iterator<Item = ShellInput> + '_ {
    bytes
        .chunks(MAX_DATA_CHUNK)
        .map(|chunk| ShellInput::Data(chunk.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{read_frame, write_frame, StreamOpen};

    #[test]
    fn sizes_are_bounded() {
        assert_eq!(TermSize::new(80, 24), Some(TermSize { cols: 80, rows: 24 }));
        assert_eq!(TermSize::new(0, 24), None);
        assert_eq!(TermSize::new(80, 0), None);
        assert_eq!(TermSize::new(MAX_DIMENSION + 1, 24), None);
        assert!(TermSize::new(MAX_DIMENSION, MAX_DIMENSION).is_some());
        assert!(!TermSize { cols: 0, rows: 1 }.is_valid());
        assert!(TermSize::default().is_valid());
    }

    #[tokio::test]
    async fn a_whole_session_round_trips_over_framing() {
        // What the server writes, then what the agent writes, on one stream.
        let open = StreamOpen::Shell {
            size: TermSize {
                cols: 100,
                rows: 40,
            },
        };
        let inputs = [
            ShellInput::Data(b"Get-Date\r".to_vec()),
            ShellInput::Resize(TermSize {
                cols: 132,
                rows: 50,
            }),
            ShellInput::Data(vec![0x03]), // Ctrl+C
        ];
        let outputs = [
            ShellOutput::Started,
            ShellOutput::Data(b"\x1b[2J\x1b[HPS C:\\> ".to_vec()),
            ShellOutput::Exited { code: Some(0) },
        ];

        let mut to_agent = Vec::new();
        write_frame(&mut to_agent, &open).await.unwrap();
        for i in &inputs {
            write_frame(&mut to_agent, i).await.unwrap();
        }
        let mut r = to_agent.as_slice();
        assert_eq!(
            read_frame::<_, StreamOpen>(&mut r).await.unwrap(),
            Some(open)
        );
        for i in &inputs {
            assert_eq!(
                read_frame::<_, ShellInput>(&mut r).await.unwrap().as_ref(),
                Some(i)
            );
        }
        assert!(read_frame::<_, ShellInput>(&mut r).await.unwrap().is_none());

        let mut to_server = Vec::new();
        for o in &outputs {
            write_frame(&mut to_server, o).await.unwrap();
        }
        let mut r = to_server.as_slice();
        for o in &outputs {
            assert_eq!(
                read_frame::<_, ShellOutput>(&mut r).await.unwrap().as_ref(),
                Some(o)
            );
        }
    }

    #[test]
    fn client_resize_is_parsed_and_validated() {
        assert_eq!(
            parse_client_control(r#"{"type":"resize","cols":132,"rows":43}"#),
            Ok(ShellInput::Resize(TermSize {
                cols: 132,
                rows: 43
            }))
        );
        assert_eq!(
            parse_client_control(r#"{"type":"resize","cols":0,"rows":43}"#),
            Err(ControlError::BadSize)
        );
        for bad in [
            "",
            "resize",
            r#"{"type":"reboot"}"#,
            r#"{"type":"resize","cols":80}"#,
            r#"{"type":"resize","cols":80,"rows":24,"extra":1}"#,
            r#"{"type":"resize","cols":-1,"rows":24}"#,
        ] {
            assert!(
                matches!(parse_client_control(bad), Err(ControlError::Malformed(_))),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn agent_frames_map_to_client_messages() {
        // The JSON is the contract with the TUI: keep it stable.
        assert_eq!(
            ShellOutput::Started.into_client_frame(),
            (ClientFrame::Text(r#"{"type":"started"}"#.into()), false)
        );
        assert_eq!(
            ShellOutput::Data(b"hi".to_vec()).into_client_frame(),
            (ClientFrame::Binary(b"hi".to_vec()), false)
        );
        assert_eq!(
            ShellOutput::Exited { code: Some(3) }.into_client_frame(),
            (
                ClientFrame::Text(r#"{"type":"exit","code":3}"#.into()),
                true
            )
        );
        assert_eq!(
            ShellOutput::Exited { code: None }.into_client_frame(),
            (
                ClientFrame::Text(r#"{"type":"exit","code":null}"#.into()),
                true
            )
        );
        assert_eq!(
            ShellOutput::Error("no ConPTY".into()).into_client_frame(),
            (
                ClientFrame::Text(r#"{"type":"error","message":"no ConPTY"}"#.into()),
                true
            )
        );
    }

    #[test]
    fn large_input_is_split_into_bounded_frames() {
        let bytes: Vec<u8> = (0..MAX_DATA_CHUNK * 2 + 5).map(|i| i as u8).collect();
        let frames: Vec<ShellInput> = data_frames(&bytes).collect();
        assert_eq!(frames.len(), 3);
        let mut joined = Vec::new();
        for f in frames {
            let ShellInput::Data(d) = f else {
                panic!("not data")
            };
            assert!(d.len() <= MAX_DATA_CHUNK);
            joined.extend(d);
        }
        assert_eq!(joined, bytes);
        assert_eq!(data_frames(&[]).count(), 0);
    }
}
