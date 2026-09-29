use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use agent::credstore::{Credential, CredentialStore};
use agent::enroll::EnrollOptions;
use agent::update::{CheckOutcome, UpdateClient, VerifiedUpdate};
use agent::updater::{self, UpdatePaths};
use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use tracing::{error, info, warn};

/// Delay before reconnecting after the connection drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(about = "RMM agent", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enroll with a one-time token from a download link and store the
    /// issued credential.
    Enroll(EnrollArgs),
    /// Connect with the stored credential, heartbeat, and apply signed updates.
    Run(RunArgs),
}

#[derive(Args)]
struct StateDir {
    /// Where the protected credential is kept.
    #[arg(long, env = "RMM_STATE_DIR", default_value_os_t = default_state_dir())]
    state_dir: PathBuf,
}

#[derive(Args)]
struct EnrollArgs {
    /// Server UDP address.
    #[arg(long, env = "RMM_SERVER", default_value = "127.0.0.1:4433")]
    server: SocketAddr,
    /// Name the server certificate must be valid for.
    #[arg(long, env = "RMM_SERVER_NAME", default_value = "localhost")]
    server_name: String,
    /// PEM CA certificate the server's certificate must chain to (ca.crt).
    #[arg(long, env = "RMM_SERVER_CA")]
    server_ca: PathBuf,
    /// Enrollment token from the download link.
    #[arg(long, env = "RMM_ENROLL_TOKEN", hide_env_values = true)]
    token: String,
    /// Replace an existing credential.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    state: StateDir,
}

#[derive(Args)]
struct RunArgs {
    /// Seconds between heartbeats.
    #[arg(long, default_value_t = agent::DEFAULT_HEARTBEAT_INTERVAL.as_secs())]
    heartbeat_secs: u64,
    /// Seconds between update checks; 0 disables them.
    #[arg(long, env = "RMM_UPDATE_INTERVAL_SECS", default_value_t = 3600)]
    update_interval_secs: u64,
    #[command(flatten)]
    state: StateDir,
}

fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(base).join("RMM").join("agent")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("agent-state")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    common::logging::init();
    match Cli::parse().command {
        Command::Enroll(args) => enroll(args).await,
        Command::Run(args) => run(args).await,
    }
}

async fn enroll(args: EnrollArgs) -> anyhow::Result<()> {
    let store = CredentialStore::new(&args.state.state_dir);
    if store.exists() && !args.force {
        bail!(
            "already enrolled (credential in {}); pass --force to enroll again",
            store.dir().display()
        );
    }
    let server_ca_pem = std::fs::read_to_string(&args.server_ca)
        .with_context(|| format!("reading {}", args.server_ca.display()))?;
    let credential = agent::enroll::enroll(&EnrollOptions {
        server_addr: args.server,
        server_name: args.server_name,
        server_ca_pem,
        token: args.token,
        bind_addr: None,
    })
    .await?;
    store.save(&credential)?;
    info!(agent_id = %credential.agent_id, state_dir = %store.dir().display(), "credential stored");
    Ok(())
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    let paths = UpdatePaths::for_current_exe().context("locating own executable")?;
    // Remove the binary replaced by the last update, if any.
    updater::cleanup(&paths);

    let store = CredentialStore::new(&args.state.state_dir);
    let credential = store.load().with_context(|| {
        format!(
            "loading credential from {} (run `agent enroll` first?)",
            store.dir().display()
        )
    })?;
    let config = credential.agent_config(Duration::from_secs(args.heartbeat_secs.max(1)))?;
    info!(agent_id = %credential.agent_id, version = env!("CARGO_PKG_VERSION"), "starting");

    let update = update_loop(&credential, Duration::from_secs(args.update_interval_secs));
    tokio::select! {
        () = connect_forever(&config) => unreachable!(),
        update = update => {
            let update = update?;
            install_and_relaunch(&paths, update)
        }
        res = tokio::signal::ctrl_c() => {
            res.context("waiting for ctrl-c")?;
            info!("shutting down");
            Ok(())
        }
    }
}

/// Connect and heartbeat, reconnecting after any failure.
async fn connect_forever(config: &agent::AgentConfig) {
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

/// Poll for updates. Returns once a verified update is ready to install;
/// never returns if updates are disabled or unavailable.
async fn update_loop(
    credential: &Credential,
    interval: Duration,
) -> anyhow::Result<VerifiedUpdate> {
    let Some(key) = agent::update::baked_public_key() else {
        warn!("auto-update disabled: this build has no RMM_UPDATE_PUBKEY");
        return std::future::pending().await;
    };
    if interval.is_zero() {
        info!("auto-update disabled by configuration");
        return std::future::pending().await;
    }
    let client = UpdateClient::new(
        &credential.api_url,
        &credential.ca_pem,
        key,
        protocol::update::current_platform(),
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("crate version is semver"),
    )?;
    loop {
        match client.check().await {
            Ok(CheckOutcome::Available(update)) => return Ok(update),
            Ok(CheckOutcome::UpToDate) => {}
            // A bad signature is logged loudly but never installed.
            Err(e) => error!("update check failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

fn install_and_relaunch(paths: &UpdatePaths, update: VerifiedUpdate) -> anyhow::Result<()> {
    info!(version = %update.version, "installing update");
    updater::stage(paths, &update.binary).context("staging update")?;
    updater::swap(paths).context("swapping in update")?;
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let err = updater::relaunch(&paths.current, &args);
    Err(err).context("relaunching after update")
}
