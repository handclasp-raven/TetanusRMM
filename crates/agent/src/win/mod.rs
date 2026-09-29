//! Windows-only parts of the agent.
//!
//! Process model once installed:
//!
//! ```text
//!  Session 0 (no desktop)                 User's session (e.g. 1)
//!  ┌──────────────────────────────┐       ┌─────────────────────────────┐
//!  │ rmm-agent.exe service run    │ spawn │ rmm-agent.exe helper        │
//!  │ LocalSystem, auto-start      │──────▶│ runs as the logged-on user  │
//!  │ - QUIC + heartbeat/telemetry │       │ - tray icon, Ctrl+F12       │
//!  │ - updates                    │◀─────▶│ - capture + encode          │
//!  │ - consent decisions          │ pipe  │ - input, clipboard          │
//!  │ - supervises the helper      │       │ - consent prompt, toasts    │
//!  └──────────────────────────────┘       └─────────────────────────────┘
//! ```
//!
//! See [`crate::session`] for why the helper exists.

pub mod acl;
pub mod bridge;
pub mod capture;
pub mod capture_test;
pub mod clipboard;
pub mod consent;
pub mod encoder;
pub mod helper;
pub mod input;
pub mod pipe;
pub mod process;
pub mod service;
pub mod stream;
pub mod toast;
