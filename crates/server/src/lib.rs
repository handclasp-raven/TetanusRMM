//! RMM central server.
//!
//! - [`quic`]: QUIC listener for agents (mutual TLS, heartbeats).
//! - [`api`]: HTTPS API for users.
//! - [`auth`], [`users`]: Argon2id passwords, TOTP, sessions, roles.
//! - [`audit`]: hash-chained audit log.
//! - [`registry`]: agents and per-device policies.
//! - [`enroll`]: enrollment tokens and the internal CA for agent certificates.
//! - [`updates`]: signing and publishing agent updates.

pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod enroll;
pub mod quic;
pub mod registry;
pub mod updates;
pub mod users;

pub use quic::{Registry, Server, ServerConfig, ServerError, ServerEvent};
