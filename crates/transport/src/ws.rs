//! The WebSocket-over-TLS fallback transport.
//!
//! A plain TLS 1.3 connection over TCP (same server certificate and the
//! same optional client certificate as QUIC, see
//! `common::quic::mutual_tls_server`), upgraded to a WebSocket at [`PATH`],
//! then multiplexed by [`crate::mux`]. It looks like ordinary HTTPS
//! WebSocket traffic, so it gets through firewalls that block UDP.
//!
//! The server listens on TCP, by default on the same port number as its
//! QUIC (UDP) listener, so one `host:port` reaches both.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::mux::{self, Side};

/// Request path of the tunnel.
pub const PATH: &str = "/rmm/v1/tunnel";

/// ALPN on the TLS connection: it is an HTTP/1.1 upgrade.
pub const ALPN: &[u8] = b"http/1.1";

/// Budget for TCP connect + TLS + WebSocket handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest WebSocket message accepted. Mux frames are at most
/// `mux::MAX_CHUNK` plus a header.
const MAX_MESSAGE: usize = 256 * 1024;

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("connecting: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid server name {0:?}")]
    ServerName(String),
    #[error("websocket handshake: {0}")]
    Handshake(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("handshake timed out")]
    Timeout,
}

/// Client end: connect to `addr`, verify the server as `server_name` with
/// `tls` (see `common::quic::mutual_tls_client`, with [`ALPN`]), and upgrade.
pub async fn connect(
    addr: SocketAddr,
    server_name: &str,
    tls: Arc<rustls::ClientConfig>,
) -> Result<mux::Connection, WsError> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let name = ServerName::try_from(server_name.to_owned())
            .map_err(|_| WsError::ServerName(server_name.to_owned()))?;
        let stream = TlsConnector::from(tls).connect(name, tcp).await?;
        let host = match addr {
            SocketAddr::V6(_) if server_name.parse::<std::net::Ipv6Addr>().is_ok() => {
                format!("[{server_name}]")
            }
            _ => server_name.to_owned(),
        };
        let url = format!("wss://{host}:{}{PATH}", addr.port());
        let (ws, _response) =
            tokio_tungstenite::client_async_with_config(url, stream, Some(ws_config())).await?;
        Ok(mux::spawn(ws, Side::Client, addr))
    })
    .await
    .map_err(|_| WsError::Timeout)?
}

/// An accepted TLS connection, before the WebSocket upgrade.
pub struct Accepted<S> {
    pub stream: tokio_rustls::server::TlsStream<S>,
    pub remote: SocketAddr,
}

impl<S> Accepted<S> {
    /// The client's certificate chain, if it presented one (already
    /// verified against the client CA by the TLS layer).
    pub fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        self.stream.get_ref().1.peer_certificates()
    }
}

/// Server end, step 1: the TLS handshake.
pub async fn accept_tls<S>(
    acceptor: &TlsAcceptor,
    tcp: S,
    remote: SocketAddr,
) -> Result<Accepted<S>, WsError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
        .await
        .map_err(|_| WsError::Timeout)??;
    Ok(Accepted { stream, remote })
}

/// Server end, step 2: the WebSocket upgrade. Anything but a WebSocket
/// request for [`PATH`] is refused with 404.
pub async fn upgrade<S>(accepted: Accepted<S>) -> Result<mux::Connection, WsError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    #[allow(clippy::result_large_err)] // the callback signature is tungstenite's
    fn check_path(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        if request.uri().path() == PATH {
            Ok(response)
        } else {
            let mut refusal = ErrorResponse::new(Some("not found".into()));
            *refusal.status_mut() = StatusCode::NOT_FOUND;
            Err(refusal)
        }
    }
    let ws = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_hdr_async_with_config(
            accepted.stream,
            check_path,
            Some(ws_config()),
        ),
    )
    .await
    .map_err(|_| WsError::Timeout)??;
    Ok(mux::spawn(ws, Side::Server, accepted.remote))
}

/// TLS acceptor for the tunnel listener.
pub fn acceptor(config: rustls::ServerConfig) -> TlsAcceptor {
    TlsAcceptor::from(Arc::new(config))
}
