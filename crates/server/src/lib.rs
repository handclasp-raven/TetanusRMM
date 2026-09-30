//! RMM central server.
//!
//! - [`quic`]: QUIC listener for agents (mutual TLS, heartbeats).
//! - [`api`]: HTTPS API for users.
//! - [`auth`], [`users`]: Argon2id passwords, TOTP, sessions, roles.
//! - [`access`]: role- and grant-based access to agents (RBAC).
//! - [`groups`]: agent groups.
//! - [`metrics`]: Prometheus metrics.
//! - [`audit`]: hash-chained audit log.
//! - [`registry`]: agents and per-device policies.
//! - [`enroll`]: enrollment tokens and the internal CA for agent certificates.
//! - [`updates`]: signing and publishing agent updates.
//! - [`relay`]: fan-out of agent video to viewers, and routing of each
//!   session's end-to-end sealed records (the server cannot read either).
//! - [`stun`]: STUN responder, for agents and viewers gathering public
//!   addresses to connect directly.
//! - [`viewers`]: short-lived viewer-session tokens.
//! - [`remote`]: remote shell, script runner and file transfer, relayed to
//!   agents for API clients (no viewer needed).

pub mod access;
pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod enroll;
pub mod groups;
pub mod metrics;
pub mod quic;
pub mod registry;
pub mod relay;
pub mod remote;
pub mod stun;
pub mod updates;
pub mod users;
pub mod viewers;

pub use quic::{Registry, Server, ServerConfig, ServerError, ServerEvent};
