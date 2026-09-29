//! Connecting to the server with fallback.
//!
//! In [`TransportMode::Auto`] a client tries QUIC first, giving the
//! handshake [`QUIC_ATTEMPT`] (UDP that is silently dropped otherwise only
//! shows up as a 30 s idle timeout), then the WebSocket. Only failing to
//! *connect* falls back: a server that answers and then refuses (bad
//! certificate, bad token) refuses on either transport.
//!
//! Once QUIC has failed and the WebSocket worked, [`Preference`] tries the
//! WebSocket first for [`STICKY`], so an agent on a UDP-blocked network
//! reconnects promptly; afterwards QUIC gets another chance, in case the
//! agent moved to a better network.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Identity, TlsError};
use rustls::pki_types::CertificateDer;
use tracing::{info, warn};

use crate::{ws, Connection, TransportKind, TransportMode};

/// How long an automatic QUIC attempt gets before trying the WebSocket.
pub const QUIC_ATTEMPT: Duration = Duration::from_secs(5);

/// How long to prefer the WebSocket after QUIC failed.
pub const STICKY: Duration = Duration::from_secs(60 * 60);

/// Where and how to connect.
#[derive(Debug, Clone)]
pub struct Target {
    /// Server UDP address (QUIC).
    pub quic_addr: SocketAddr,
    /// Server TCP address (WebSocket over TLS).
    pub ws_addr: SocketAddr,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    pub mode: TransportMode,
    /// Local UDP address for QUIC; `None` binds the unspecified address of
    /// the server's family (what production clients want, see
    /// `agent::AgentSession::rebind`).
    pub bind: Option<SocketAddr>,
}

/// A connected transport.
pub struct Dialed {
    pub connection: Connection,
    /// The QUIC endpoint, kept for the connection's lifetime (and for
    /// migration). `None` on the WebSocket.
    pub endpoint: Option<quinn::Endpoint>,
}

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    /// Every transport tried failed; one message per attempt.
    #[error("{}", .0.join("; "))]
    Unreachable(Vec<String>),
}

/// Which transport to try first, remembered across reconnects.
#[derive(Debug, Clone, Default)]
pub struct Preference {
    websocket_until: Option<Instant>,
}

impl Preference {
    /// Transports to try, in order.
    pub fn order(&self, mode: TransportMode, now: Instant) -> Vec<TransportKind> {
        match mode {
            TransportMode::Quic => vec![TransportKind::Quic],
            TransportMode::WebSocket => vec![TransportKind::WebSocket],
            TransportMode::Auto if self.websocket_until.is_some_and(|t| now < t) => {
                vec![TransportKind::WebSocket, TransportKind::Quic]
            }
            TransportMode::Auto => vec![TransportKind::Quic, TransportKind::WebSocket],
        }
    }

    /// A connection on `kind` succeeded; `quic_failed` if QUIC was tried
    /// and failed first.
    pub fn connected(&mut self, kind: TransportKind, quic_failed: bool, now: Instant) {
        match kind {
            TransportKind::Quic => self.websocket_until = None,
            TransportKind::WebSocket if quic_failed => {
                self.websocket_until = Some(now + STICKY);
            }
            TransportKind::WebSocket => {}
        }
    }
}

/// Connects to one server, per [`Target`].
pub struct Dialer {
    target: Target,
    quic: quinn::ClientConfig,
    ws_tls: Arc<rustls::ClientConfig>,
}

impl Dialer {
    /// Trust only `server_ca`; present `identity` if given (agents do,
    /// viewers and enrolling agents do not).
    pub fn new(
        target: Target,
        server_ca: &[CertificateDer<'static>],
        identity: Option<&Identity>,
    ) -> Result<Self, TlsError> {
        Ok(Self {
            quic: common::quic::client_config(server_ca, identity)?,
            ws_tls: Arc::new(common::quic::mutual_tls_client(
                server_ca,
                identity,
                ws::ALPN,
            )?),
            target,
        })
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    /// Connect using the first transport that works.
    pub async fn dial(&self, preference: &mut Preference) -> Result<Dialed, DialError> {
        let order = preference.order(self.target.mode, Instant::now());
        let auto = order.len() > 1;
        let mut failures = Vec::new();
        let mut quic_failed = false;
        for kind in order {
            let attempt = match kind {
                TransportKind::Quic => self.quic(auto).await,
                TransportKind::WebSocket => self.websocket().await,
            };
            match attempt {
                Ok(dialed) => {
                    if quic_failed {
                        warn!(
                            server = %self.target.ws_addr,
                            "QUIC (UDP) is unreachable; connected over the WebSocket fallback"
                        );
                    } else {
                        info!(transport = %kind, "connected");
                    }
                    preference.connected(kind, quic_failed, Instant::now());
                    return Ok(dialed);
                }
                Err(e) => {
                    quic_failed |= kind == TransportKind::Quic;
                    failures.push(format!("{kind}: {e}"));
                }
            }
        }
        Err(DialError::Unreachable(failures))
    }

    async fn quic(&self, bounded: bool) -> Result<Dialed, String> {
        let addr = self.target.quic_addr;
        let bind = self.target.bind.unwrap_or(match addr {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        });
        let mut endpoint =
            quinn::Endpoint::client(bind).map_err(|e| format!("binding UDP {bind}: {e}"))?;
        endpoint.set_default_client_config(self.quic.clone());
        let connecting = endpoint
            .connect(addr, &self.target.server_name)
            .map_err(|e| e.to_string())?;
        let connection = if bounded {
            tokio::time::timeout(QUIC_ATTEMPT, connecting)
                .await
                .map_err(|_| format!("no answer within {}s", QUIC_ATTEMPT.as_secs()))?
        } else {
            connecting.await
        }
        .map_err(|e| e.to_string())?;
        Ok(Dialed {
            connection: Connection::Quic(connection),
            endpoint: Some(endpoint),
        })
    }

    async fn websocket(&self) -> Result<Dialed, String> {
        let connection = ws::connect(
            self.target.ws_addr,
            &self.target.server_name,
            self.ws_tls.clone(),
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(Dialed {
            connection: Connection::WebSocket(connection),
            endpoint: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_modes_use_one_transport() {
        let p = Preference::default();
        let now = Instant::now();
        assert_eq!(p.order(TransportMode::Quic, now), [TransportKind::Quic]);
        assert_eq!(
            p.order(TransportMode::WebSocket, now),
            [TransportKind::WebSocket]
        );
    }

    #[test]
    fn auto_prefers_quic_until_it_fails_then_sticks_to_websocket_for_a_while() {
        let mut p = Preference::default();
        let t0 = Instant::now();
        let quic_first = [TransportKind::Quic, TransportKind::WebSocket];
        let ws_first = [TransportKind::WebSocket, TransportKind::Quic];
        assert_eq!(p.order(TransportMode::Auto, t0), quic_first);

        // QUIC failed, the WebSocket worked.
        p.connected(TransportKind::WebSocket, true, t0);
        assert_eq!(p.order(TransportMode::Auto, t0), ws_first);
        // Reconnecting over the WebSocket keeps the original deadline...
        p.connected(TransportKind::WebSocket, false, t0 + STICKY / 2);
        assert_eq!(p.order(TransportMode::Auto, t0 + STICKY / 2), ws_first);
        // ...after which QUIC is probed again.
        assert_eq!(p.order(TransportMode::Auto, t0 + STICKY), quic_first);

        // QUIC working again clears the preference at once.
        p.connected(TransportKind::WebSocket, true, t0);
        p.connected(TransportKind::Quic, false, t0 + Duration::from_secs(1));
        assert_eq!(
            p.order(TransportMode::Auto, t0 + Duration::from_secs(2)),
            quic_first
        );
    }
}
