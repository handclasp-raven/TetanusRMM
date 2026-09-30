//! End-to-end encrypted sessions between agent and viewer, and direct
//! peer-to-peer paths for them (Phase 10).
//!
//! A remote-desktop session starts on the server's relay, as before, but
//! its content is sealed end to end:
//!
//! - [`noise`]: the session's keys, from a Noise handshake that
//!   authenticates the agent by its enrolled certificate and the viewer by
//!   the key the server bound to its session; control records (input,
//!   clipboard, signaling) sealed under them.
//! - [`media`]: video sealed once under a shared key, so the relay can
//!   still fan one encode out to every viewer without reading it.
//!
//! Then, when the network allows, it moves to a direct path:
//!
//! - [`gather`]: candidate addresses for a UDP socket: host addresses, and
//!   the public (server-reflexive) one the server's STUN responder sees.
//! - [`direct`]: hole punching and a QUIC connection between the peers,
//!   each pinning the other's per-attempt certificate, which it learned
//!   over the sealed session.
//! - [`merge`]: the viewer's ordering of video that arrives over both
//!   paths while it switches, so the switch shows no glitch.
//!
//! Neither end re-keys when the path changes: the same sealed records and
//! frames flow over the relay or the direct path, and back to the relay if
//! the direct path drops.

pub mod direct;
pub mod gather;
pub mod media;
pub mod merge;
pub mod noise;

pub use direct::DirectSettings;
pub use noise::{E2eError, Session, StaticKey};
