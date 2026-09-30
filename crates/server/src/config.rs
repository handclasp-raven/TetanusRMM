//! Runtime configuration for `server serve`, from flags or environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Args;
use common::devcerts;
use common::{Identity, TlsError};

use crate::enroll::AgentCa;

#[derive(Debug, Clone, Args)]
pub struct ServeConfig {
    /// UDP address for agent (QUIC) connections.
    #[arg(long, env = "RMM_QUIC_LISTEN", default_value = "0.0.0.0:4433")]
    pub quic_listen: SocketAddr,

    /// TCP address for the WebSocket-over-TLS fallback, for agents and
    /// viewers on networks that block UDP. Same port number as QUIC by
    /// default, so clients need only one `host:port`.
    #[arg(long, env = "RMM_WS_LISTEN", default_value = "0.0.0.0:4433")]
    pub ws_listen: SocketAddr,

    /// Disable the WebSocket fallback (QUIC only).
    #[arg(long, env = "RMM_NO_WEBSOCKET")]
    pub no_websocket: bool,

    /// UDP address of the STUN responder that agents and viewers use to
    /// learn their public addresses for direct connections. Clients reach it
    /// on the server's own address, at this port.
    #[arg(long, env = "RMM_STUN_LISTEN", default_value = "0.0.0.0:3478")]
    pub stun_listen: SocketAddr,

    /// The STUN port to announce, if clients reach the responder on a
    /// different port than it listens on (e.g. published elsewhere by Docker).
    #[arg(long, env = "RMM_STUN_ANNOUNCE_PORT")]
    pub stun_announce_port: Option<u16>,

    /// Keep every session on the relay: tell agents and viewers not to try
    /// direct paths (and do not run the STUN responder).
    #[arg(long, env = "RMM_NO_DIRECT")]
    pub no_direct: bool,

    /// Serve Prometheus metrics over plain HTTP at /metrics on this address
    /// (e.g. 127.0.0.1:9464). Off unless set. Reachable only by the
    /// monitoring system: it needs no credentials.
    #[arg(long, env = "RMM_METRICS_LISTEN")]
    pub metrics_listen: Option<SocketAddr>,

    /// TCP address for the HTTPS API.
    #[arg(long, env = "RMM_API_LISTEN", default_value = "0.0.0.0:8443")]
    pub api_listen: SocketAddr,

    /// Postgres connection URL, e.g. postgres://user:pass@host:5432/db.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// Upper bound on pooled database connections.
    #[arg(long, env = "RMM_DB_MAX_CONNECTIONS", default_value_t = 10)]
    pub db_max_connections: u32,

    /// Directory holding ca.crt, server.crt and server.key (from `gen-certs`).
    /// Used for the QUIC listener, and for HTTPS unless the API cert is set.
    #[arg(long, env = "RMM_CERTS_DIR", default_value = "dev-certs")]
    pub certs_dir: PathBuf,

    /// PEM certificate chain for the HTTPS API (e.g. from a public CA).
    #[arg(long, env = "RMM_API_TLS_CERT", requires = "api_tls_key")]
    pub api_tls_cert: Option<PathBuf>,

    /// PEM private key for the HTTPS API.
    #[arg(long, env = "RMM_API_TLS_KEY", requires = "api_tls_cert")]
    pub api_tls_key: Option<PathBuf>,

    /// Lifetime of a user login session, in seconds.
    #[arg(long, env = "RMM_SESSION_TTL_SECS", default_value_t = 12 * 60 * 60)]
    pub session_ttl_secs: u64,

    /// Base URL agents and users reach the HTTPS API at. Used in download
    /// links and handed to agents at enrollment.
    #[arg(long, env = "RMM_PUBLIC_URL", default_value = "https://localhost:8443")]
    pub public_url: String,

    /// Directory of published agent updates (see `publish-update`).
    #[arg(long, env = "RMM_UPDATES_DIR", default_value = "updates")]
    pub updates_dir: PathBuf,
}

impl ServeConfig {
    /// Where the WebSocket fallback listens, unless it is disabled.
    pub fn ws_listen(&self) -> Option<SocketAddr> {
        (!self.no_websocket).then_some(self.ws_listen)
    }

    /// Certificate presented to agents over QUIC.
    pub fn quic_identity(&self) -> Result<Identity, TlsError> {
        Identity::from_files(
            &self.certs_dir.join(devcerts::SERVER_CERT),
            &self.certs_dir.join(devcerts::SERVER_KEY),
        )
    }

    /// CA that agent client certificates must chain to.
    pub fn agent_ca(&self) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, TlsError> {
        common::tls::load_certs(&self.certs_dir.join(devcerts::CA_CERT))
    }

    /// Internal CA that signs agent certificates at enrollment: `ca.crt` and
    /// `ca.key` in the certs dir.
    pub fn enrollment_ca(&self) -> anyhow::Result<AgentCa> {
        let read = |name: &str| {
            let path = self.certs_dir.join(name);
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
        };
        Ok(AgentCa::from_pem(
            &read(devcerts::CA_CERT)?,
            &read(devcerts::CA_KEY)?,
        )?)
    }

    /// Public URL without a trailing slash.
    pub fn public_url(&self) -> String {
        self.public_url.trim_end_matches('/').to_owned()
    }

    /// Certificate presented to HTTPS clients: the explicit API cert if set,
    /// otherwise the same dev server certificate used for QUIC.
    pub fn api_identity(&self) -> Result<Identity, TlsError> {
        match (&self.api_tls_cert, &self.api_tls_key) {
            (Some(cert), Some(key)) => Identity::from_files(cert, key),
            _ => self.quic_identity(),
        }
    }

    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.session_ttl_secs)
    }
}
