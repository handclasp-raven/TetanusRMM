//! Runtime configuration for `server serve`, from flags or environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use common::devcerts;
use common::{Identity, TlsError};

#[derive(Debug, Clone, Args)]
pub struct ServeConfig {
    /// UDP address for agent (QUIC) connections.
    #[arg(long, env = "RMM_QUIC_LISTEN", default_value = "0.0.0.0:4433")]
    pub quic_listen: SocketAddr,

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
}

impl ServeConfig {
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
