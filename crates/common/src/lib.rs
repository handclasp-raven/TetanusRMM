//! Helpers shared by the RMM binaries: logging setup, TLS/QUIC configuration and
//! development certificate generation.

pub mod devcerts;
pub mod logging;
pub mod quic;
pub mod tls;

pub use tls::{Identity, TlsError};
