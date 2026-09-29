//! RMM central server.
//!
//! - [`quic`]: QUIC listener for agents (mutual TLS, heartbeats).
//! - [`api`]: HTTPS API for users.
//! - [`auth`], [`users`]: Argon2id passwords, TOTP, sessions, roles.
//! - [`audit`]: hash-chained audit log.
//! - [`registry`]: agents and per-device policies.

pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod db;
pub mod quic;
pub mod registry;
pub mod users;

pub use quic::{Server, ServerConfig, ServerError, ServerEvent};
