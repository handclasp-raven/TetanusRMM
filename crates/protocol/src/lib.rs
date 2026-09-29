//! Wire protocol shared by the agent, server and viewer.
//!
//! Every message on a QUIC stream is a [`Message`] encoded with `postcard` and
//! prefixed by its length as a big-endian `u32` (see [`framing`]).

pub mod clipboard;
pub mod consent;
pub mod framing;
pub mod input;
pub mod ipc;
pub mod media;
pub mod script;
pub mod shell;
pub mod transfer;
pub mod update;

use serde::{Deserialize, Serialize};

pub use framing::{read_frame, write_frame, FrameError, MAX_FRAME_LEN};

/// Protocol version carried in [`Message::Hello`]. Bump on incompatible changes.
///
/// - 1: Phase 1-3.
/// - 2: `Heartbeat` gained `telemetry`. A version-1 agent's heartbeats no
///   longer decode, so it is disconnected; it can still self-update over HTTPS.
/// - 3: screen streaming messages (appended variants only; a version-2 agent
///   still works, it just cannot stream).
/// - 4: consent, remote input and clipboard (appended variants). Viewing an
///   agent older than 4 is refused, since it cannot enforce consent.
/// - 5: remote shell, script runner and file transfer, each on a stream the
///   server opens (see [`StreamOpen`]). Older agents never accept those
///   streams, so the server refuses these operations for them.
/// - 6: `AgentInfo` (appended variant): the agent reports its hostname.
/// - 7: adaptive bitrate (appended variants): `StreamReport` to agents,
///   `EnableFrameAcks` to viewers and `FrameAck` from them. Each is only
///   sent to a peer that said it speaks version 7, so older agents and
///   viewers keep working at a fixed bitrate.
pub const PROTOCOL_VERSION: u32 = 7;

/// Oldest agent and viewer protocol that takes part in adaptive bitrate.
pub const MIN_ADAPTIVE_VERSION: u32 = 7;

/// Oldest agent protocol that serves [`StreamOpen`] streams.
pub const MIN_REMOTE_OPS_VERSION: u32 = 5;

/// ALPN identifier negotiated during the TLS handshake. Both ends must agree.
pub const ALPN: &[u8] = b"rmm/1";

/// Every message that can appear on the wire.
///
/// Variants are append-only: postcard encodes the variant index, so reordering
/// or removing variants breaks compatibility with deployed agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// First message an agent sends on its control stream.
    Hello { agent_id: String, version: u32 },
    /// Periodic liveness signal from the agent. `ts` is Unix time in
    /// milliseconds. `telemetry` is the latest health sample, if available.
    Heartbeat {
        ts: u64,
        seq: u64,
        telemetry: Option<Telemetry>,
    },
    /// Server acknowledgement of the heartbeat with the same `seq`.
    HeartbeatAck { seq: u64 },
    /// First (and only) message on a connection made *without* a client
    /// certificate: exchange a one-time enrollment token for a certificate.
    /// `csr_der` is a PKCS#10 request signed by the agent's freshly generated
    /// key; the private key never leaves the agent.
    Enroll { token: String, csr_der: Vec<u8> },
    /// Server reply to a successful [`Message::Enroll`]. The agent must
    /// reconnect using `cert_pem` as its client certificate.
    Enrolled {
        agent_id: String,
        cert_pem: String,
        /// CA that signed `cert_pem` and the server's certificate.
        ca_pem: String,
        /// Base URL of the server's HTTPS API (for updates).
        api_url: String,
    },

    // --- Screen streaming (Phase 5) -------------------------------------
    //
    // Viewers connect to the server, never to the agent (agents are behind
    // NAT). The server relays; see `media` for the video streams themselves.
    /// Viewer's first message on its control stream: a short-lived,
    /// single-use viewer-session token from the HTTPS API.
    ViewerHello { token: String, version: u32 },
    /// Server to viewer after a valid `ViewerHello`.
    ViewerWelcome {
        agent_id: String,
        monitors: Vec<media::MonitorInfo>,
        /// Monitor currently being streamed, if a stream is running.
        active_monitor: Option<u32>,
    },
    /// Agent to server (and server to viewers): the agent's displays.
    /// Sent in reply to `ListMonitors` and whenever they change.
    MonitorList { monitors: Vec<media::MonitorInfo> },
    /// Server to agent: send a fresh `MonitorList`.
    ListMonitors,
    /// Viewer to server: show this monitor. The server turns it into
    /// `StartStream` for the agent; the choice is shared by all viewers of
    /// that agent, since the agent encodes a single stream.
    SelectMonitor { monitor: u32 },
    /// Server to agent: capture and encode `monitor`, sending frames on a new
    /// unidirectional stream. Replaces any stream already running.
    StartStream { monitor: u32 },
    /// Server to agent: no viewers left; stop capturing.
    StopStream,
    /// Server to agent (or viewer to server): encode a keyframe as soon as
    /// possible, e.g. because a viewer joined mid-stream.
    RequestKeyframe,
    /// Server to viewer: the stream now shows `monitor` (another viewer may
    /// have switched it).
    StreamMonitor { monitor: u32 },

    // --- Consent, input and clipboard (Phase 6) -------------------------
    //
    // A viewer's session only starts once the agent has applied the device's
    // consent policy (see `consent`). Input and clipboard then flow
    // viewer -> server -> agent, tagged by the server with the session id so
    // the agent can drop anything for a session it has not granted or has
    // ended.
    /// Agent to server, after `Hello`: what kind of device this is (picks
    /// the default consent mode).
    DeviceInfo { kind: consent::DeviceKind },
    /// Server to agent: a technician wants a session; apply the policy.
    SessionRequest(consent::SessionRequest),
    /// Agent to server: how the request ended. The session starts only if
    /// `outcome.allows_session()`.
    SessionDecision {
        session_id: u64,
        outcome: consent::Outcome,
    },
    /// Server to agent: the session ended (or was abandoned while pending).
    SessionEnded { session_id: u64 },
    /// Server to viewer, before `ViewerWelcome`: waiting for the user to
    /// answer a consent prompt for up to `timeout_secs`.
    ConsentPending { timeout_secs: u32 },
    /// Viewer to server: inject this input.
    Input(input::InputEvent),
    /// Server to agent: input from `session_id`'s technician.
    SessionInput {
        session_id: u64,
        event: input::InputEvent,
    },
    /// Viewer to server: the technician's clipboard changed. Agent to
    /// server, and server to viewers: the remote user's clipboard changed.
    Clipboard(clipboard::ClipboardData),
    /// Server to agent: clipboard content from `session_id`'s technician.
    SessionClipboard {
        session_id: u64,
        data: clipboard::ClipboardData,
    },
    /// Agent to server: the user pressed the Ctrl+F12 kill switch; end every
    /// session with this agent now.
    UserTerminatedSessions,

    // --- Agent details (Phase 8) ----------------------------------------
    /// Agent to server, after `DeviceInfo`: the machine's hostname, shown to
    /// technicians. At most [`MAX_HOSTNAME_LEN`] bytes; the server trims and
    /// cleans it (see `sanitize_hostname`).
    AgentInfo { hostname: String },

    // --- Adaptive bitrate (Phase 9) -------------------------------------
    //
    // The agent encodes one stream for every viewer, so its bitrate must
    // suit its own uplink *and* the slowest viewer's downlink. Both are
    // measured the same way: acknowledgements of frame sequence numbers
    // give round trips on the sender's own clock, and a round trip above
    // the recent minimum is time spent queued. See `media::StreamReport`.
    /// Server to agent (version 7+), a few times a second while frames
    /// arrive: delivery of the agent's video, and the viewers' delay.
    StreamReport(media::StreamReport),
    /// Server to viewer (version 7+), after `ViewerWelcome`: acknowledge
    /// frames with `FrameAck`. Viewers never send `FrameAck` unasked, so a
    /// new viewer does not upset an older server.
    EnableFrameAcks,
    /// Viewer to server, periodically once enabled: the newest frame
    /// received.
    FrameAck { seq: u64 },
}

/// Longest hostname the server stores, in bytes (the DNS limit).
pub const MAX_HOSTNAME_LEN: usize = 253;

/// A hostname fit for display: control characters removed, surrounding
/// whitespace trimmed, cut to [`MAX_HOSTNAME_LEN`] bytes on a character
/// boundary. `None` if nothing is left.
pub fn sanitize_hostname(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let mut name = cleaned.trim();
    if name.len() > MAX_HOSTNAME_LEN {
        let mut end = MAX_HOSTNAME_LEN;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name = name[..end].trim_end();
    }
    (!name.is_empty()).then(|| name.to_owned())
}

/// First frame on a bidirectional stream the **server** opens to an agent.
/// Each remote operation gets its own stream, so a large file transfer never
/// delays a shell or the control stream. What follows depends on the kind:
/// see [`shell`], [`script`] and [`transfer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamOpen {
    /// Interactive PowerShell on a pseudoconsole of this size.
    Shell { size: shell::TermSize },
    /// Run a script without a terminal and reply with its result.
    Script(script::ScriptRequest),
    /// Write a file on the agent.
    Upload(transfer::UploadRequest),
    /// Read a file from the agent. `path` is absolute.
    Download { path: String },
}

/// Agent health sample, sent on each heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Telemetry {
    /// Whole-machine CPU usage since the previous sample, 0-100.
    pub cpu_percent: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Used and total bytes of the system disk (the one holding the OS).
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub uptime_secs: u64,
}

/// QUIC application close codes shared by both ends.
pub mod close_code {
    /// Orderly shutdown.
    pub const NORMAL: u32 = 0;
    /// The peer sent a message that is not valid at this point in the protocol.
    pub const PROTOCOL_ERROR: u32 = 1;
    /// Enrollment token rejected, or client certificate not (or no longer)
    /// registered to the claimed agent.
    pub const UNAUTHORIZED: u32 = 2;
    /// A viewer asked for an agent that is not connected (or disconnected).
    pub const AGENT_OFFLINE: u32 = 3;
    /// The consent policy refused the session (declined, timed out, or
    /// nobody to ask). The close reason says which.
    pub const CONSENT_REFUSED: u32 = 4;
    /// The user ended the session with the Ctrl+F12 kill switch.
    pub const USER_TERMINATED: u32 = 5;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Telemetry {
        Telemetry {
            cpu_percent: 37.25,
            mem_used_bytes: 6_442_450_944,
            mem_total_bytes: 17_179_869_184,
            disk_used_bytes: 120_000_000_000,
            disk_total_bytes: 512_000_000_000,
            uptime_secs: 3_600,
        }
    }

    #[test]
    fn telemetry_round_trips_through_postcard() {
        let msg = Message::Heartbeat {
            ts: 1,
            seq: 2,
            telemetry: Some(sample()),
        };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
    }

    #[test]
    fn heartbeat_without_telemetry_costs_one_byte() {
        let with = postcard::to_stdvec(&Message::Heartbeat {
            ts: 1,
            seq: 2,
            telemetry: None,
        })
        .unwrap();
        // variant index + ts + seq + Option tag
        assert_eq!(with.len(), 4);
    }

    #[test]
    fn version_1_heartbeat_does_not_decode_as_version_2() {
        // Phase 1-3 agents sent Heartbeat { ts, seq } with no telemetry field.
        #[derive(Serialize)]
        enum V1 {
            _Hello,
            Heartbeat { ts: u64, seq: u64 },
        }
        let old = postcard::to_stdvec(&V1::Heartbeat { ts: 1, seq: 2 }).unwrap();
        assert!(postcard::from_bytes::<Message>(&old).is_err());
    }

    #[test]
    fn agent_info_is_appended_after_every_earlier_variant() {
        let msg = Message::AgentInfo {
            hostname: "DESKTOP-42".into(),
        };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        // Variant 24: indices 0-23 are unchanged for older peers.
        assert_eq!(bytes[0], 24);
        assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
    }

    #[test]
    fn adaptive_bitrate_messages_are_appended_after_agent_info() {
        let msgs = [
            Message::StreamReport(media::StreamReport {
                seq: 7,
                bytes: 1 << 20,
                viewer_delay_ms: 35,
            }),
            Message::EnableFrameAcks,
            Message::FrameAck { seq: 99 },
        ];
        for (i, msg) in msgs.into_iter().enumerate() {
            let bytes = postcard::to_stdvec(&msg).unwrap();
            assert_eq!(usize::from(bytes[0]), 25 + i);
            assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn hostnames_are_cleaned_for_display() {
        assert_eq!(sanitize_hostname("  WS-01\r\n"), Some("WS-01".into()));
        assert_eq!(sanitize_hostname("a\u{1b}[31mb"), Some("a[31mb".into()));
        assert_eq!(sanitize_hostname(" \t\n"), None);
        assert_eq!(sanitize_hostname(""), None);
        // Cut to the limit without splitting a multi-byte character.
        let long = "é".repeat(200); // 400 bytes
        let cut = sanitize_hostname(&long).unwrap();
        assert!(cut.len() <= MAX_HOSTNAME_LEN);
        assert_eq!(cut.len(), 252);
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
