//! End-to-end encrypted sessions between agent and viewer (Phase 10).
//!
//! Everything a technician's session carries (video, input, clipboard, and
//! the signaling for a direct path) is encrypted by the agent or viewer
//! before it reaches any transport, and decrypted only by the other end.
//! The server relays these as opaque bytes:
//!
//! - control records travel in [`crate::Message::Sealed`], whose `data` is a
//!   postcard [`Envelope`];
//! - video travels as [`crate::media::MediaFrame`]s whose `payload` is
//!   sealed with the agent's media key (see [`MediaKey`]); the relay still
//!   reads the frame's `seq`/`keyframe` header, which the seal
//!   authenticates.
//!
//! The session is keyed by a Noise `XX` handshake (in the `peer` crate)
//! between the viewer's per-connection static key, which the server binds
//! to the session it authorised, and the agent's static key, which the
//! agent signs with its enrolled certificate ([`AgentProof`]). The same
//! keys protect the session on the relay and on a direct path, so moving
//! between them never re-keys.
//!
//! What the server still sees is metadata it needs to route: who is in a
//! session with which agent, frame sequence numbers and sizes, and whether
//! the session is relayed or direct ([`Path`]).

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::clipboard::ClipboardData;
use crate::input::InputEvent;
use crate::media::{StreamSettings, StreamStatus};

/// Length of every symmetric key and X25519 public key in a session.
pub const KEY_LEN: usize = 32;

/// Largest [`crate::Message::Sealed`] payload the server relays: a full
/// clipboard plus framing and authentication overhead.
pub const MAX_SEALED_LEN: usize = crate::clipboard::MAX_CLIPBOARD_BYTES + 64 * 1024;

/// What a `Message::Sealed` carries. Opaque to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Envelope {
    /// One Noise handshake message: viewer -> agent, agent -> viewer
    /// (carrying the [`AgentProof`]), viewer -> agent.
    Handshake(Vec<u8>),
    /// A sealed [`Control`] record. `nonce` counts up from 0 per direction
    /// and is never reused under one key; the receiver uses it to reject
    /// replays and to restore order across the relay and direct paths.
    Record { nonce: u64, ciphertext: Vec<u8> },
}

/// The plaintext of a sealed record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Control {
    /// Viewer -> agent: inject this input.
    Input(InputEvent),
    /// Either way: the local clipboard changed.
    Clipboard(ClipboardData),
    /// Agent -> viewer: the key video is sealed with from `epoch` on.
    MediaKey(MediaKey),
    /// Viewer -> agent: let's try a direct path.
    DirectOffer(DirectOffer),
    /// Agent -> viewer: ready; connect to one of these.
    DirectAnswer(DirectAnswer),
    /// Either way: attempt `attempt` is over without a direct path (or the
    /// agent will not try, e.g. direct paths are disabled on its side).
    DirectFailed { attempt: u32 },
    /// Viewer -> agent (on the direct path): video now comes over the
    /// direct path for attempt `attempt`, so the agent need not also send
    /// it through the relay for this session.
    DirectInUse { attempt: u32 },
    /// Viewer -> agent (on the direct path): the newest frame received,
    /// for adaptive bitrate while the relay is out of the loop.
    FrameAck { seq: u64 },
    /// Viewer -> agent: this technician's video settings (sent on connect
    /// and when changed).
    StreamSettings(StreamSettings),
    /// Agent -> viewer: how the video is streamed now. Only to viewers
    /// that sent `StreamSettings` (older viewers do not know this record).
    StreamStatus(StreamStatus),
}

/// A video key. Keys change when a viewer leaves, so a departed viewer
/// cannot read what follows even if something still delivers it frames.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaKey {
    pub epoch: u32,
    pub key: [u8; KEY_LEN],
}

impl std::fmt::Debug for MediaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MediaKey {{ epoch: {}, .. }}", self.epoch)
    }
}

/// Viewer -> agent: start direct attempt `attempt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectOffer {
    pub attempt: u32,
    /// Where the viewer can be reached: host and server-reflexive (STUN)
    /// addresses of the UDP socket it will connect from.
    pub candidates: Vec<SocketAddr>,
    /// SHA-256 of the self-signed certificate the viewer presents on the
    /// direct QUIC connection. The agent accepts no other.
    pub cert_sha256: [u8; 32],
}

/// Agent -> viewer: the agent has punched towards the viewer's candidates
/// and is listening on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectAnswer {
    pub attempt: u32,
    pub candidates: Vec<SocketAddr>,
    /// SHA-256 of the agent's self-signed certificate for this attempt.
    pub cert_sha256: [u8; 32],
}

/// First frame the viewer writes on the bidirectional stream it opens on a
/// direct connection. Sealed `Envelope::Record`s follow, both ways; the
/// agent sends video on a unidirectional stream, as on the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectHello {
    pub attempt: u32,
}

/// Handshake payload from the agent: its enrolled certificate chain and a
/// signature by the certificate's key over [`agent_proof_message`], which
/// ties the agent's Noise static key to its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProof {
    /// DER, leaf first.
    pub cert_chain: Vec<Vec<u8>>,
    /// TLS `SignatureScheme` code of `signature`.
    pub scheme: u16,
    pub signature: Vec<u8>,
}

/// What an [`AgentProof`] signs.
pub fn agent_proof_message(agent_id: &str, static_key: &[u8]) -> Vec<u8> {
    let mut msg = b"rmm e2e agent static key v1\0".to_vec();
    msg.extend_from_slice(agent_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(static_key);
    msg
}

/// How a session's traffic flows, as the viewer reports it to the server
/// (for metrics and the audit log; it never affects authorisation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    /// Through the server.
    Relayed,
    /// Straight between agent and viewer.
    Direct,
    /// A direct attempt failed; the session stays relayed.
    DirectFailed,
}

impl Path {
    pub fn as_str(self) -> &'static str {
        match self {
            Path::Relayed => "relayed",
            Path::Direct => "direct",
            Path::DirectFailed => "direct_failed",
        }
    }
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_keys_never_show_up_in_logs() {
        let key = MediaKey {
            epoch: 3,
            key: [0xAB; KEY_LEN],
        };
        let shown = format!("{:?}", Control::MediaKey(key));
        assert!(shown.contains("epoch: 3"));
        assert!(!shown.to_lowercase().contains("ab, "), "{shown}");
        assert!(!shown.contains("171"), "{shown}");
    }

    #[test]
    fn the_proof_message_separates_agent_id_from_key() {
        // "agt-1" + key "2..." must not collide with "agt-12" + key "...".
        let a = agent_proof_message("agt-1", b"2abc");
        let b = agent_proof_message("agt-12", b"abc");
        assert_ne!(a, b);
    }

    #[test]
    fn paths_serialise_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&Path::DirectFailed).unwrap(),
            "\"direct_failed\""
        );
        assert_eq!(Path::Direct.to_string(), "direct");
    }
}
