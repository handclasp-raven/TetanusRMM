//! Windows-only parts of the agent.
//!
//! Process model once installed:
//!
//! ```text
//!  Session 0 (no desktop)                 User's session (e.g. 1)
//!  ┌──────────────────────────────┐       ┌─────────────────────────────┐
//!  │ rmm-agent.exe service run    │ spawn │ rmm-agent.exe helper        │
//!  │ LocalSystem, auto-start      │──────▶│ runs as the logged-on user  │
//!  │ - QUIC + heartbeat/telemetry │       │ - tray, indicator, Ctrl+F12 │
//!  │ - updates                    │◀─────▶│ - capture + encode          │
//!  │ - consent decisions          │ pipe  │ - input, clipboard          │
//!  │ - supervises the helpers     │       │ - consent prompt, toasts    │
//!  │                              │       └─────────────────────────────┘
//!  │                              │ spawn ┌─────────────────────────────┐
//!  │                              │──────▶│ rmm-agent.exe input-helper  │
//!  │                              │◀─────▶│ runs as SYSTEM, no windows  │
//!  └──────────────────────────────┘ pipe  │ - input into any window,    │
//!                                         │   elevated ones included    │
//!                                         └─────────────────────────────┘
//! ```
//!
//! See [`crate::session`] for why the helper exists, and [`input_helper`]
//! for why input has a process of its own.

pub mod acl;
pub mod bridge;
pub mod capture;
pub mod capture_test;
pub mod clipboard;
pub mod conpty;
pub mod consent;
pub mod encoder;
pub mod gdi;
pub mod helper;
pub mod indicator;
pub mod input;
pub mod input_helper;
pub mod pipe;
pub mod process;
pub mod service;
pub mod stream;
pub mod toast;
pub mod wgc;
