use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use agent::AgentConfig;
use anyhow::Context;
use clap::Parser;
use common::devcerts;
use common::Identity;
use tracing::{info, warn};

/// Delay before reconnecting after the connection drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(about = "RMM agent")]
struct Cli {
    /// Server UDP address.
    #[arg(long, env = "RMM_SERVER", default_value = "127.0.0.1:4433")]
    server: SocketAddr,
    /// Name the server certificate must be valid for.
    #[arg(long, env = "RMM_SERVER_NAME", default_value = "localhost")]
    server_name: String,
    /// Identifier sent in Hello. Should match the agent certificate's CN.
    #[arg(long, env = "RMM_AGENT_ID", default_value = "dev-agent-1")]
    agent_id: String,
    /// Directory holding ca.crt, agent.crt and agent.key.
    #[arg(long, env = "RMM_CERTS_DIR", default_value = "dev-certs")]
    certs_dir: PathBuf,
    /// Seconds between heartbeats.
    #[arg(long, default_value_t = agent::DEFAULT_HEARTBEAT_INTERVAL.as_secs())]
    heartbeat_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    common::logging::init();
    let cli = Cli::parse();
    let dir = &cli.certs_dir;
    let config = AgentConfig {
        server_addr: cli.server,
        server_name: cli.server_name,
        agent_id: cli.agent_id,
        server_ca: common::tls::load_certs(&dir.join(devcerts::CA_CERT))
            .context("loading server CA (run `cargo run -p server -- gen-certs` first?)")?,
        identity: Identity::from_files(
            &dir.join(devcerts::AGENT_CERT),
            &dir.join(devcerts::AGENT_KEY),
        )
        .context("loading agent certificate")?,
        heartbeat_interval: Duration::from_secs(cli.heartbeat_secs.max(1)),
        bind_addr: None,
    };

    tokio::select! {
        () = run_forever(&config) => unreachable!(),
        res = tokio::signal::ctrl_c() => res.context("waiting for ctrl-c")?,
    }
    info!("shutting down");
    Ok(())
}

/// Connect and heartbeat, reconnecting after any failure.
async fn run_forever(config: &AgentConfig) {
    loop {
        match agent::connect(config).await {
            Ok(session) => {
                if let Err(e) = session.run(None).await {
                    warn!("connection lost: {e}");
                }
                session.close();
            }
            Err(e) => warn!("connect failed: {e}"),
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}
