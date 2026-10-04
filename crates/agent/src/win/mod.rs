//! Windows-only parts of the agent.
//!
//! Process model once installed:
//!
//! ```text
//!  Session 0 (no desktop)                 Console session (e.g. 1)
//!  ┌──────────────────────────────┐       ┌─────────────────────────────┐
//!  │ rmm-agent.exe service run    │ spawn │ rmm-agent.exe helper        │
//!  │ LocalSystem, auto-start      │──────▶│ runs as the logged-on user  │
//!  │ - QUIC + heartbeat/telemetry │       │ - tray, indicator, Ctrl+F12 │
//!  │ - updates                    │◀─────▶│ - capture + encode          │
//!  │ - consent decisions          │ pipe  │ - clipboard                 │
//!  │ - supervises the helpers     │       │ - consent prompt, toasts    │
//!  │ - Ctrl+Alt+Del               │       │ - lent-password prompt      │
//!  │ - keeps the lent password    │       └─────────────────────────────┘
//!  │                              │ spawn ┌─────────────────────────────┐
//!  │                              │──────▶│ rmm-agent.exe system-helper │
//!  │                              │◀─────▶│ runs as SYSTEM, no windows  │
//!  └──────────────────────────────┘ pipe  │ - input into any window,    │
//!                                         │   elevated ones included    │
//!                                         │ - capture + input on the    │
//!                                         │   logon and lock screens    │
//!                                         │   and UAC prompts           │
//!                                         └─────────────────────────────┘
//! ```
//!
//! The session helper exists while a user is logged on at the console; the
//! system helper whenever there is a console session, so also at the logon
//! screen.
//!
//! The quick assist client has none of this: one process, run by the user,
//! does it all for a single session (see [`local`]).
//!
//! See [`crate::session`] for why the helper exists, and [`system_helper`]
//! for why there is a second one running as SYSTEM.

pub mod acl;
pub mod bridge;
pub mod capture;
pub mod capture_test;
pub mod clipboard;
pub mod conpty;
pub mod consent;
pub mod credential;
pub mod desktop;
pub mod encoder;
pub mod flyout;
pub mod gdi;
pub mod helper;
pub mod indicator;
pub mod input;
pub mod local;
pub mod pipe;
pub mod process;
pub mod sas;
pub mod secret;
pub mod service;
pub mod stream;
pub mod system_helper;
pub mod toast;
pub mod ui;
pub mod wgc;
