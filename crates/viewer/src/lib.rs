//! Remote desktop viewer (cross-platform: Linux, macOS, Windows).
//!
//! The viewer never talks to the agent directly: agents sit behind NAT. It
//! connects to the server with a short-lived viewer-session token, and the
//! server relays the agent's video (fanned out to every viewer from one
//! encode).
//!
//! - [`client`]: QUIC connection to the server and the viewer protocol.
//! - [`decode`]: H.264 decoding with OpenH264. It is portable, unlike Media
//!   Foundation, so the viewer builds everywhere.
//! - [`render`]: scaling into the window and the on-screen monitor picker.
//! - [`keymap`]: physical keys to the scancodes the agent injects.
//! - [`clipboard`]: two-way clipboard sync with the remote machine.

pub mod client;
pub mod clipboard;
pub mod decode;
pub mod keymap;
pub mod render;
