//! Windows-only parts of the agent.
//!
//! Process model once installed:
//!
//! ```text
//!  Session 0 (no desktop)                 User's session (e.g. 1)
//!  ┌──────────────────────────────┐       ┌─────────────────────────────┐
//!  │ rmm-agent.exe service run    │ spawn │ rmm-agent.exe helper        │
//!  │ LocalSystem, auto-start      │──────▶│ runs as the logged-on user  │
//!  │ - QUIC + heartbeat/telemetry │       │ - tray icon                 │
//!  │ - updates                    │◀─────▶│ - (later) capture + input   │
//!  │ - supervises the helper      │ pipe  │                             │
//!  └──────────────────────────────┘       └─────────────────────────────┘
//! ```
//!
//! See [`crate::session`] for why the helper exists.

pub mod acl;
pub mod bridge;
pub mod capture;
pub mod capture_test;
pub mod encoder;
pub mod helper;
pub mod pipe;
pub mod process;
pub mod service;
pub mod stream;
