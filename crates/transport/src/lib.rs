//! Connections between agents/viewers and the server.
//!
//! QUIC is the transport of choice (see `common::quic`), but it is UDP, and
//! some networks block outbound UDP. So the server also accepts the same
//! protocol over a WebSocket on TLS/TCP, multiplexed by [`mux`] into the
//! same kinds of streams. Everything above this crate (control stream,
//! video, shells, file transfers) works unchanged on either.
//!
//! - [`Connection`], [`SendStream`], [`RecvStream`]: one API over both.
//! - [`ws`]: the WebSocket-over-TLS client and server ends.
//! - [`dial`]: connecting with fallback: QUIC first, WebSocket if UDP does
//!   not get through, and remembering that for a while.

pub mod dial;
pub mod mux;
pub mod ws;

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub use dial::{Dialed, Dialer, Preference};

/// Which transport a connection runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Quic,
    #[serde(rename = "websocket")]
    WebSocket,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Quic => "quic",
            TransportKind::WebSocket => "websocket",
        }
    }
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which transports a client may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    /// QUIC, falling back to WebSocket when QUIC cannot connect.
    #[default]
    Auto,
    /// QUIC only.
    Quic,
    /// WebSocket only (known UDP-blocked networks).
    #[serde(rename = "websocket")]
    WebSocket,
}

impl std::str::FromStr for TransportMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(TransportMode::Auto),
            "quic" => Ok(TransportMode::Quic),
            "websocket" | "ws" => Ok(TransportMode::WebSocket),
            other => Err(format!(
                "unknown transport {other:?} (expected auto, quic or websocket)"
            )),
        }
    }
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TransportMode::Auto => "auto",
            TransportMode::Quic => "quic",
            TransportMode::WebSocket => "websocket",
        })
    }
}

/// A client's transport settings, as configured and stored (agents keep
/// them in their credential).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportSettings {
    #[serde(default)]
    pub mode: TransportMode,
    /// TCP address of the server's WebSocket fallback. `None`: the QUIC
    /// address (same host and port number, over TCP), which is where the
    /// server listens by default.
    #[serde(default)]
    pub ws_addr: Option<SocketAddr>,
}

impl TransportSettings {
    /// Where to connect for a server whose QUIC address is `quic_addr`.
    pub fn target(
        &self,
        quic_addr: SocketAddr,
        server_name: &str,
        bind: Option<SocketAddr>,
    ) -> dial::Target {
        dial::Target {
            quic_addr,
            ws_addr: self.ws_addr.unwrap_or(quic_addr),
            server_name: server_name.to_owned(),
            mode: self.mode,
            bind,
        }
    }
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionError {
    /// The peer closed it with an application code and reason (see
    /// `protocol::close_code`).
    #[error("closed by peer (code {code}): {reason}")]
    ApplicationClosed { code: u32, reason: String },
    #[error("closed locally")]
    LocallyClosed,
    #[error("timed out")]
    TimedOut,
    #[error("connection failed: {0}")]
    Transport(String),
}

impl ConnectionError {
    /// The peer's reason, if it closed the connection deliberately.
    pub fn application_reason(&self) -> Option<&str> {
        match self {
            ConnectionError::ApplicationClosed { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

impl From<quinn::ConnectionError> for ConnectionError {
    fn from(e: quinn::ConnectionError) -> Self {
        match e {
            quinn::ConnectionError::ApplicationClosed(close) => {
                ConnectionError::ApplicationClosed {
                    code: u32::try_from(close.error_code.into_inner()).unwrap_or(u32::MAX),
                    reason: String::from_utf8_lossy(&close.reason).into_owned(),
                }
            }
            quinn::ConnectionError::LocallyClosed => ConnectionError::LocallyClosed,
            quinn::ConnectionError::TimedOut => ConnectionError::TimedOut,
            other => ConnectionError::Transport(other.to_string()),
        }
    }
}

/// A connection on either transport. Cheap to clone.
#[derive(Clone)]
pub enum Connection {
    Quic(quinn::Connection),
    WebSocket(mux::Connection),
}

impl From<quinn::Connection> for Connection {
    fn from(c: quinn::Connection) -> Self {
        Connection::Quic(c)
    }
}

impl From<mux::Connection> for Connection {
    fn from(c: mux::Connection) -> Self {
        Connection::WebSocket(c)
    }
}

impl Connection {
    pub fn kind(&self) -> TransportKind {
        match self {
            Connection::Quic(_) => TransportKind::Quic,
            Connection::WebSocket(_) => TransportKind::WebSocket,
        }
    }

    /// The QUIC connection, if that is what this is.
    pub fn as_quic(&self) -> Option<&quinn::Connection> {
        match self {
            Connection::Quic(c) => Some(c),
            Connection::WebSocket(_) => None,
        }
    }

    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        Ok(match self {
            Connection::Quic(c) => {
                let (s, r) = c.open_bi().await?;
                (SendStream::Quic(s), RecvStream::Quic(r))
            }
            Connection::WebSocket(c) => {
                let (s, r) = c.open_bi().await?;
                (SendStream::WebSocket(s), RecvStream::WebSocket(r))
            }
        })
    }

    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        Ok(match self {
            Connection::Quic(c) => {
                let (s, r) = c.accept_bi().await?;
                (SendStream::Quic(s), RecvStream::Quic(r))
            }
            Connection::WebSocket(c) => {
                let (s, r) = c.accept_bi().await?;
                (SendStream::WebSocket(s), RecvStream::WebSocket(r))
            }
        })
    }

    pub async fn open_uni(&self) -> Result<SendStream, ConnectionError> {
        Ok(match self {
            Connection::Quic(c) => SendStream::Quic(c.open_uni().await?),
            Connection::WebSocket(c) => SendStream::WebSocket(c.open_uni().await?),
        })
    }

    pub async fn accept_uni(&self) -> Result<RecvStream, ConnectionError> {
        Ok(match self {
            Connection::Quic(c) => RecvStream::Quic(c.accept_uni().await?),
            Connection::WebSocket(c) => RecvStream::WebSocket(c.accept_uni().await?),
        })
    }

    /// Close with an application `code` (see `protocol::close_code`) and a
    /// human-readable `reason` for the peer.
    pub fn close(&self, code: u32, reason: &[u8]) {
        match self {
            Connection::Quic(c) => c.close(code.into(), reason),
            Connection::WebSocket(c) => c.close(code, reason),
        }
    }

    /// Wait until the connection ends.
    pub async fn closed(&self) -> ConnectionError {
        match self {
            Connection::Quic(c) => c.closed().await.into(),
            Connection::WebSocket(c) => c.closed().await,
        }
    }

    /// Why the connection ended, if it has.
    pub fn close_reason(&self) -> Option<ConnectionError> {
        match self {
            Connection::Quic(c) => c.close_reason().map(Into::into),
            Connection::WebSocket(c) => c.close_reason(),
        }
    }

    /// The peer's address (for QUIC, the current path's).
    pub fn remote_address(&self) -> SocketAddr {
        match self {
            Connection::Quic(c) => c.remote_address(),
            Connection::WebSocket(c) => c.remote_address(),
        }
    }

    /// Identifies the connection in logs.
    pub fn stable_id(&self) -> usize {
        match self {
            Connection::Quic(c) => c.stable_id(),
            Connection::WebSocket(c) => c.stable_id(),
        }
    }
}

/// The writing half of a stream on either transport.
pub enum SendStream {
    Quic(quinn::SendStream),
    WebSocket(mux::SendStream),
}

impl SendStream {
    /// No more data; the peer reads end of stream after what was written.
    /// Errors if already finished or reset.
    pub fn finish(&mut self) -> Result<(), ClosedStream> {
        match self {
            SendStream::Quic(s) => s.finish().map_err(|_| ClosedStream),
            SendStream::WebSocket(s) => s.finish().map_err(|_| ClosedStream),
        }
    }

    /// Abandon the stream: the peer gets an error rather than end of stream.
    pub fn reset(&mut self, code: u32) -> Result<(), ClosedStream> {
        match self {
            SendStream::Quic(s) => s.reset(code.into()).map_err(|_| ClosedStream),
            SendStream::WebSocket(s) => s.reset(code).map_err(|_| ClosedStream),
        }
    }
}

/// The stream was already finished or reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stream already finished or reset")]
pub struct ClosedStream;

impl AsyncWrite for SendStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            SendStream::Quic(s) => Pin::new(s).poll_write(cx, buf).map_err(io::Error::from),
            SendStream::WebSocket(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            SendStream::Quic(s) => Pin::new(s).poll_flush(cx),
            SendStream::WebSocket(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            SendStream::Quic(s) => Pin::new(s).poll_shutdown(cx),
            SendStream::WebSocket(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// The reading half of a stream on either transport.
pub enum RecvStream {
    Quic(quinn::RecvStream),
    WebSocket(mux::RecvStream),
}

impl AsyncRead for RecvStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            RecvStream::Quic(s) => Pin::new(s).poll_read(cx, buf),
            RecvStream::WebSocket(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_modes_parse_and_display() {
        for (text, mode) in [
            ("auto", TransportMode::Auto),
            ("QUIC", TransportMode::Quic),
            ("websocket", TransportMode::WebSocket),
            ("ws", TransportMode::WebSocket),
        ] {
            assert_eq!(text.parse::<TransportMode>(), Ok(mode));
        }
        assert!("tcp".parse::<TransportMode>().is_err());
        assert_eq!(TransportMode::WebSocket.to_string(), "websocket");
        assert_eq!(
            serde_json::to_string(&TransportKind::WebSocket).unwrap(),
            "\"websocket\""
        );
    }
}
