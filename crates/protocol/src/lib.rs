//! Wire protocol shared by the agent, server and viewer.
//!
//! Every message on a QUIC stream is a [`Message`] encoded with `postcard` and
//! prefixed by its length as a big-endian `u32` (see [`framing`]).

pub mod framing;
pub mod update;

use serde::{Deserialize, Serialize};

pub use framing::{read_frame, write_frame, FrameError, MAX_FRAME_LEN};

/// Protocol version carried in [`Message::Hello`]. Bump on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// ALPN identifier negotiated during the TLS handshake. Both ends must agree.
pub const ALPN: &[u8] = b"rmm/1";

/// Every message that can appear on the wire.
///
/// Variants are append-only: postcard encodes the variant index, so reordering
/// or removing variants breaks compatibility with deployed agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// First message an agent sends on its control stream.
    Hello { agent_id: String, version: u32 },
    /// Periodic liveness signal from the agent. `ts` is Unix time in milliseconds.
    Heartbeat { ts: u64, seq: u64 },
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
