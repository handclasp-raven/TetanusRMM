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
//!   primitives: text in the built-in typeface, rounded boxes, icons.
//! - [`ui`]: the header, the tab rail with its panels and the footer
//!   around the picture: display mode, monitors, the session's buttons,
//!   the vault, the agent's status, command buttons and file transfer.
//! - [`theme`]: the colours, which follow the TUI's theme.
//! - [`icons`]: the icons on the buttons and tabs.
//! - [`api`]: the server's HTTPS API behind the panels.
//! - [`keymap`]: physical keys to the scancodes the agent injects.
//! - [`clipboard`]: two-way clipboard sync with the remote machine.
//! - [`vault`]: the technician's Bitwarden vault, through the `bw` client,
//!   for usernames, passwords and one-time codes to type remotely.

pub mod api;
pub mod client;
pub mod clipboard;
pub mod decode;
pub mod icons;
pub mod keymap;
pub mod render;
pub mod theme;
pub mod ui;
pub mod vault;
