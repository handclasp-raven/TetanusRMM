//! Remote desktop viewer (cross-platform: Linux, macOS, Windows).
//!
//! The viewer connects to the server with a short-lived viewer-session
//! token, and the session starts on the server's relay (agents sit behind
//! NAT; the relay fans one encode out to every viewer). Everything in the
//! session is end-to-end encrypted between viewer and agent, and when the
//! network allows, the session moves to a direct connection to the agent
//! (NAT traversal), falling back to the relay if that drops.
//!
//! - [`client`]: the connection to the server (and the agent), the viewer
//!   protocol, end-to-end encryption and direct paths.
//! - [`decode`]: H.264 decoding with OpenH264. It is portable, unlike Media
//!   Foundation, so the viewer builds everywhere.
//! - [`render`]: placing the picture (scale, stretch or fill) and drawing
//!   primitives with a bitmap font.
//! - [`ui`]: the toolbar and side panel around the picture: display mode,
//!   monitors, the session's buttons, the vault, the agent's status,
//!   command buttons and file transfer.
//! - [`api`]: the server's HTTPS API behind the side panel.
//! - [`keymap`]: physical keys to the scancodes the agent injects.
//! - [`clipboard`]: two-way clipboard sync with the remote machine.
//! - [`vault`]: the technician's Bitwarden vault, through the `bw` client,
//!   for usernames, passwords and one-time codes to type remotely.

pub mod api;
pub mod client;
pub mod clipboard;
pub mod decode;
pub mod keymap;
pub mod render;
pub mod ui;
pub mod vault;
