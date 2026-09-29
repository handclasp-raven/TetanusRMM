//! Wire protocol shared by the agent, server and viewer.
//!
//! Every message on a QUIC stream is a [`Message`] encoded with `postcard` and
//! prefixed by its length as a big-endian `u32` (see [`framing`]).

pub mod framing;
pub mod ipc;
pub mod update;

use serde::{Deserialize, Serialize};

pub use framing::{read_frame, write_frame, FrameError, MAX_FRAME_LEN};

/// Protocol version carried in [`Message::Hello`]. Bump on incompatible changes.
///
/// - 1: Phase 1-3.
/// - 2: `Heartbeat` gained `telemetry`. A version-1 agent's heartbeats no
///   longer decode, so it is disconnected; it can still self-update over HTTPS.
pub const PROTOCOL_VERSION: u32 = 2;

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
}
