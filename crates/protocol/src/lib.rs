//! Wire protocol shared by the agent, server and viewer.
//!
//! Every message on a QUIC stream is a [`Message`] encoded with `postcard` and
//! prefixed by its length as a big-endian `u32` (see [`framing`]).

pub mod assist;
pub mod clipboard;
pub mod consent;
pub mod e2e;
pub mod framing;
pub mod input;
pub mod ipc;
pub mod launch;
pub mod media;
pub mod script;
pub mod shell;
pub mod stun;
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
/// - 8: end-to-end encrypted sessions and direct paths (appended variants,
///   see [`e2e`]). Session content is sealed between agent and viewer, so
///   a session needs both ends at version 8: the server refuses older
///   viewers, and refuses to connect viewers to older agents.
/// - 9: agent status (appended variants): the server sends
///   `EnableStatusReports` to agents at version 9+, which then report
///   `AgentStatus` (signed-in users, local address, OS, DNS servers, disks)
///   and again whenever it changes. Agents never send it unasked, so an
///   older server is not upset. Also `StreamOpen::Launch` (appended
///   variant): start a program on the signed-in user's desktop.
/// - 10: quick assist (appended variant): `AssistEnroll` trades a six-digit
///   code for a short-lived certificate (see [`assist`]). Only the quick
///   assist client sends it; an older server closes the connection.
/// - 11: the logon screen (appended sealed record): viewers may send
///   `Control::SecureAttention` for Ctrl+Alt+Del. Agents skip sealed
///   records they do not know, so an older agent just ignores it.
pub const PROTOCOL_VERSION: u32 = 11;

/// Oldest agent protocol that reports [`AgentStatus`].
pub const MIN_STATUS_VERSION: u32 = 9;

/// Oldest agent protocol that serves [`StreamOpen::Launch`].
pub const MIN_LAUNCH_VERSION: u32 = 9;

/// Oldest agent and viewer protocol with end-to-end encrypted sessions;
/// the oldest that may take part in a remote-desktop session at all.
pub const MIN_E2E_VERSION: u32 = 8;

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

    // --- End-to-end encryption and direct paths (Phase 10) ---------------
    //
    // See `e2e`. Input and clipboard no longer travel as `Input` /
    // `Clipboard` / `SessionInput` / `SessionClipboard` (those remain for
    // the wire format only): they are sealed records inside `Sealed`, as
    // is the signaling for a direct path.
    /// Viewer to server, right after `ViewerHello`: the X25519 static key
    /// the viewer will use in this session's Noise handshake.
    ViewerKey { public: [u8; e2e::KEY_LEN] },
    /// Server to agent, just before `SessionRequest`: the static key of the
    /// viewer in `session_id`. The agent completes a handshake only with
    /// that key, so nobody else can take over the session.
    SessionViewerKey {
        session_id: u64,
        public: [u8; e2e::KEY_LEN],
    },
    /// Between agent and server (tagged with the session) and viewer and
    /// server (`session_id` is ignored and set by the server): an opaque
    /// `e2e::Envelope`, relayed unread. At most `e2e::MAX_SEALED_LEN`.
    Sealed { session_id: u64, data: Vec<u8> },
    /// Server to agent (after `Hello`) and viewer (after `ViewerWelcome`):
    /// whether to try direct paths, and the UDP port of the server's STUN
    /// responder (on the server's address) for gathering public addresses.
    PeerConfig {
        direct: bool,
        stun_port: Option<u16>,
    },
    /// Viewer to server: how this session's traffic now flows. Recorded in
    /// metrics and the audit log; `Direct` also stops the relay forwarding
    /// video to this viewer, and `Relayed` restarts it (from a keyframe).
    PathReport(e2e::Path),

    // --- Agent status (Phase 11) ------------------------------------------
    /// Server to agent (version 9+), after `Hello`: send `AgentStatus` now
    /// and whenever it changes.
    EnableStatusReports,
    /// Agent to server, once enabled: who is signed in and where the agent
    /// is on its own network. The server cleans it (see
    /// [`AgentStatus::sanitized`]).
    AgentStatus(AgentStatus),

    // --- Quick assist (Phase 12) -------------------------------------------
    /// First (and only) message on a connection made *without* a client
    /// certificate by the quick assist client: exchange the six-digit code
    /// a technician read out for a short-lived certificate. Like
    /// [`Message::Enroll`], and answered with [`Message::Enrolled`].
    AssistEnroll { code: String, csr_der: Vec<u8> },
}

/// Most signed-in users an [`AgentStatus`] carries.
pub const MAX_STATUS_USERS: usize = 32;
/// Longest user name kept, in bytes.
pub const MAX_USER_NAME_LEN: usize = 256;
/// Longest OS description kept, in bytes.
pub const MAX_OS_LEN: usize = 128;
/// Most DNS servers an [`AgentStatus`] carries.
pub const MAX_DNS_SERVERS: usize = 16;
/// Most disks an [`AgentStatus`] carries.
pub const MAX_DISKS: usize = 32;
/// Longest disk name (mount point) kept, in bytes.
pub const MAX_DISK_NAME_LEN: usize = 128;

/// One mounted disk and how full it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// Where it is mounted: `C:\` on Windows, `/home` elsewhere.
    pub name: String,
    pub total_bytes: u64,
    /// At most `total_bytes`. Agents round it (see
    /// [`DiskUsage::rounded`]) so that a busy disk does not make every
    /// status check a change worth reporting.
    pub used_bytes: u64,
}

impl DiskUsage {
    /// `used_bytes` rounded down to a thousandth of the disk (0.1%).
    pub fn rounded(mut self) -> DiskUsage {
        let step = (self.total_bytes / 1000).max(1);
        self.used_bytes = self.used_bytes.min(self.total_bytes) / step * step;
        self
    }
}

/// An agent's state beyond telemetry, shown to technicians.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentStatus {
    /// Users signed in to the machine's interactive sessions (console or
    /// remote desktop), e.g. `CORP\alice`, without duplicates.
    pub users: Vec<String>,
    /// The agent's own address on its route to the server: its LAN
    /// address when it is behind NAT.
    pub local_ip: Option<std::net::IpAddr>,
    /// Operating system name and version, e.g. `Windows 11 Pro (build
    /// 26100)`. `None` if the agent cannot tell.
    pub os: Option<String>,
    /// DNS servers configured on the machine's active network adapters, in
    /// the order they are used, without duplicates.
    pub dns_servers: Vec<std::net::IpAddr>,
    /// Fixed disks (not network or removable media).
    pub disks: Vec<DiskUsage>,
}

impl AgentStatus {
    /// Fit for display and storage: user names cleaned like hostnames,
    /// cut to [`MAX_USER_NAME_LEN`], de-duplicated, at most
    /// [`MAX_STATUS_USERS`] of them; the OS cleaned the same way, cut to
    /// [`MAX_OS_LEN`]; at most [`MAX_DNS_SERVERS`] distinct DNS servers; at
    /// most [`MAX_DISKS`] disks with names cleaned and used space no more
    /// than the total.
    pub fn sanitized(&self) -> AgentStatus {
        let mut users: Vec<String> = Vec::new();
        for user in &self.users {
            if users.len() == MAX_STATUS_USERS {
                break;
            }
            if let Some(user) = clean_for_display(user, MAX_USER_NAME_LEN) {
                if !users.contains(&user) {
                    users.push(user);
                }
            }
        }
        let mut dns_servers = Vec::new();
        for ip in &self.dns_servers {
            if dns_servers.len() == MAX_DNS_SERVERS {
                break;
            }
            if !dns_servers.contains(ip) {
                dns_servers.push(*ip);
            }
        }
        let disks = self
            .disks
            .iter()
            .filter_map(|d| {
                Some(DiskUsage {
                    name: clean_for_display(&d.name, MAX_DISK_NAME_LEN)?,
                    total_bytes: d.total_bytes,
                    used_bytes: d.used_bytes.min(d.total_bytes),
                })
            })
            .take(MAX_DISKS)
            .collect();
        AgentStatus {
            users,
            local_ip: self.local_ip,
            os: self
                .os
                .as_deref()
                .and_then(|os| clean_for_display(os, MAX_OS_LEN)),
            dns_servers,
            disks,
        }
    }
}

/// Longest hostname the server stores, in bytes (the DNS limit).
pub const MAX_HOSTNAME_LEN: usize = 253;

/// A hostname fit for display: control characters removed, surrounding
/// whitespace trimmed, cut to [`MAX_HOSTNAME_LEN`] bytes on a character
/// boundary. `None` if nothing is left.
pub fn sanitize_hostname(raw: &str) -> Option<String> {
    clean_for_display(raw, MAX_HOSTNAME_LEN)
}

fn clean_for_display(raw: &str, max_len: usize) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let mut name = cleaned.trim();
    if name.len() > max_len {
        let mut end = max_len;
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
/// see [`shell`], [`script`], [`transfer`] and [`launch`].
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
    /// Start a program on the signed-in user's desktop, as that user
    /// (version 9+); the agent replies with one [`launch::LaunchReply`].
    Launch(launch::LaunchRequest),
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
    /// The peer (or the agent it asked for) speaks a protocol too old for
    /// what it asked. The close reason says which.
    pub const OUTDATED: u32 = 6;
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
    fn end_to_end_messages_are_appended_after_frame_ack() {
        let msgs = [
            Message::ViewerKey { public: [1; 32] },
            Message::SessionViewerKey {
                session_id: 4,
                public: [2; 32],
            },
            Message::Sealed {
                session_id: 4,
                data: vec![9; 100],
            },
            Message::PeerConfig {
                direct: true,
                stun_port: Some(3478),
            },
            Message::PathReport(e2e::Path::Direct),
        ];
        for (i, msg) in msgs.into_iter().enumerate() {
            let bytes = postcard::to_stdvec(&msg).unwrap();
            assert_eq!(usize::from(bytes[0]), 28 + i);
            assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn assist_enroll_is_appended_after_agent_status() {
        let msg = Message::AssistEnroll {
            code: "482913".into(),
            csr_der: vec![1, 2, 3],
        };
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(bytes[0], 35);
        assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
    }

    #[test]
    fn a_full_clipboard_fits_in_a_sealed_message_and_a_frame() {
        assert!(e2e::MAX_SEALED_LEN < MAX_FRAME_LEN as usize);
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

    #[test]
    fn agent_status_round_trips_and_is_cleaned() {
        let status = AgentStatus {
            users: vec![
                " CORP\\alice\r\n".into(),
                "CORP\\alice".into(),
                "".into(),
                "bob".into(),
                "x".repeat(300),
            ],
            local_ip: Some("192.168.1.20".parse().unwrap()),
            os: Some(" Windows 11 Pro\n".into()),
            dns_servers: vec![
                "192.168.1.1".parse().unwrap(),
                "8.8.8.8".parse().unwrap(),
                "192.168.1.1".parse().unwrap(),
            ],
            disks: vec![
                DiskUsage {
                    name: "C:\\".into(),
                    total_bytes: 500,
                    used_bytes: 200,
                },
                DiskUsage {
                    name: " \n".into(),
                    total_bytes: 1,
                    used_bytes: 1,
                },
                DiskUsage {
                    name: "D:\\".into(),
                    total_bytes: 100,
                    used_bytes: 900,
                },
            ],
        };
        let msg = Message::AgentStatus(status.clone());
        let bytes = postcard::to_stdvec(&msg).unwrap();
        assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);

        let clean = status.sanitized();
        assert_eq!(clean.users.len(), 3);
        assert_eq!(clean.users[..2], ["CORP\\alice", "bob"]);
        assert_eq!(clean.users[2].len(), MAX_USER_NAME_LEN);
        assert_eq!(clean.local_ip, status.local_ip);
        assert_eq!(clean.os.as_deref(), Some("Windows 11 Pro"));
        assert_eq!(clean.dns_servers, status.dns_servers[..2]);
        // Nameless disks are dropped; used never exceeds total.
        assert_eq!(
            clean.disks,
            [
                DiskUsage {
                    name: "C:\\".into(),
                    total_bytes: 500,
                    used_bytes: 200
                },
                DiskUsage {
                    name: "D:\\".into(),
                    total_bytes: 100,
                    used_bytes: 100
                },
            ]
        );
        let long_os = AgentStatus {
            os: Some("y".repeat(500)),
            ..Default::default()
        };
        assert_eq!(long_os.sanitized().os.unwrap().len(), MAX_OS_LEN);
        let blank_os = AgentStatus {
            os: Some(" \t".into()),
            ..Default::default()
        };
        assert_eq!(blank_os.sanitized().os, None);

        let many = AgentStatus {
            users: (0..100).map(|i| format!("u{i}")).collect(),
            ..Default::default()
        };
        assert_eq!(many.sanitized().users.len(), MAX_STATUS_USERS);
    }

    #[test]
    fn disk_usage_rounds_to_a_thousandth() {
        let disk = |total, used| DiskUsage {
            name: "C:\\".into(),
            total_bytes: total,
            used_bytes: used,
        };
        // 500 GB: steps of 0.5 GB, so a few MB written change nothing.
        let gb = 1_000_000_000u64;
        assert_eq!(
            disk(500 * gb, 123 * gb + 7_000_000).rounded().used_bytes,
            123 * gb
        );
        assert_eq!(
            disk(500 * gb, 123 * gb + gb / 2).rounded().used_bytes,
            123 * gb + gb / 2
        );
        // Tiny and odd disks.
        assert_eq!(disk(10, 7).rounded().used_bytes, 7);
        assert_eq!(disk(0, 5).rounded().used_bytes, 0);
    }
}
