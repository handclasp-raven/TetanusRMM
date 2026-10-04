use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent::core::CoreOptions;
use agent::credstore::CredentialStore;
use agent::enroll::EnrollOptions;
use agent::telemetry::SystemTelemetry;
use agent::update::VerifiedUpdate;
use agent::updater::{self, UpdatePaths};
use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use tokio::sync::watch;
use tracing::info;
use transport::{TransportMode, TransportSettings};

#[derive(Parser)]
#[command(about = "RMM agent", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enroll with a one-time token and store the credential (console mode).
    Enroll(EnrollArgs),
    /// Run in the foreground: connect, heartbeat, apply signed updates.
    Run(RunArgs),
    /// Manage the Windows service (Windows only).
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Session helper started by the service in the user's session (internal).
    #[command(hide = true)]
    Helper,
    /// Input and secure-desktop capture, started by the service as SYSTEM
    /// in the console session (internal).
    #[command(hide = true)]
    SystemHelper {
        /// Only this process may give it orders.
        #[arg(long)]
        service_pid: u32,
    },
    /// Capture and encode the screen to a raw .h264 file (development aid;
    /// Windows only, run in an interactive session).
    #[command(hide = true)]
    CaptureTest {
        #[arg(long, default_value_t = 0)]
        monitor: u32,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, default_value = "capture.h264")]
        out: PathBuf,
        #[arg(long, default_value_t = 30)]
        fps: u32,
        #[arg(long, default_value_t = 4_000_000)]
        bitrate: u32,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install as an auto-start LocalSystem service and start it. With
    /// --token, the service enrolls itself on first start.
    Install(InstallArgs),
    /// Stop and remove the service. Files are left in place.
    Uninstall,
    Start,
    Stop,
    /// Service entry point, invoked by the Service Control Manager (internal).
    #[command(hide = true)]
    Run,
}

#[derive(Args)]
struct ServerArgs {
    /// Server QUIC (UDP) address: `ip:port`, or `host:port` (resolved once,
    /// when installing or enrolling).
    #[arg(long, env = "RMM_SERVER", default_value = "127.0.0.1:4433", value_parser = server_addr)]
    server: SocketAddr,
    /// Name the server certificate must be valid for.
    #[arg(long, env = "RMM_SERVER_NAME", default_value = "localhost")]
    server_name: String,
    /// auto (QUIC, falling back to WebSocket over TLS when UDP is blocked),
    /// quic, or websocket. Stored with the credential.
    #[arg(long, env = "RMM_TRANSPORT", default_value_t = TransportMode::Auto)]
    transport: TransportMode,
    /// TCP address of the server's WebSocket fallback, if it is not the
    /// --server address (e.g. published on 443). Stored with the credential.
    #[arg(long, env = "RMM_WS_SERVER", value_parser = server_addr)]
    ws_server: Option<SocketAddr>,
}

fn server_addr(value: &str) -> Result<SocketAddr, String> {
    agent::resolve_server(value)
}

impl ServerArgs {
    fn transport(&self) -> TransportSettings {
        TransportSettings {
            mode: self.transport,
            ws_addr: self.ws_server,
        }
    }
}

#[derive(Args)]
struct InstallArgs {
    #[command(flatten)]
    server: ServerArgs,
    /// PEM CA certificate the server's certificate must chain to (ca.crt).
    #[arg(long, env = "RMM_SERVER_CA")]
    server_ca: Option<PathBuf>,
    /// Enrollment token from the download link.
    #[arg(long, env = "RMM_ENROLL_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// With --token: if this machine is already enrolled with that server,
    /// stay the same agent instead of enrolling again.
    #[arg(long, requires = "token")]
    keep_credential: bool,
    /// Install without starting.
    #[arg(long)]
    no_start: bool,
}

#[derive(Args)]
struct StateDir {
    /// Where the protected credential is kept.
    #[arg(long, env = "RMM_STATE_DIR", default_value_os_t = agent::paths::default_state_dir())]
    state_dir: PathBuf,
}

#[derive(Args)]
struct EnrollArgs {
    #[command(flatten)]
    server: ServerArgs,
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        // These two set up their own logging (to files) and runtimes.
        Command::Service(ServiceCommand::Run) => service_run(),
        Command::Helper => helper(),
        Command::SystemHelper { service_pid } => system_helper(service_pid),
        Command::CaptureTest {
            monitor,
            seconds,
            out,
            fps,
            bitrate,
        } => capture_test(monitor, seconds, &out, fps, bitrate),
        command => {
            common::logging::init();
            tokio::runtime::Runtime::new()?.block_on(console(command))
        }
    }
}

async fn console(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Enroll(args) => enroll(args).await,
        Command::Run(args) => run(args).await,
        Command::Service(command) => service(command),
        Command::Helper | Command::SystemHelper { .. } | Command::CaptureTest { .. } => {
            unreachable!()
        }
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
        transport: args.server.transport(),
        server_addr: args.server.server,
        server_name: args.server.server_name,
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
    info!(agent_id = %credential.agent_id, version = agent::core::version(), "starting");

    let (status, _) = watch::channel(agent::core::initial_status(Some(
        credential.agent_id.clone(),
    )));
    let options = CoreOptions {
        heartbeat_interval: Duration::from_secs(args.heartbeat_secs.max(1)),
        update_interval: Duration::from_secs(args.update_interval_secs),
        store: Some(store),
        telemetry: Some(Arc::new(SystemTelemetry::new())),
        media: None,
        // Console mode has no helper: no consent prompt can be shown, so
        // `require` falls back to its on_no_user setting.
        desktop: None,
        remote: Default::default(),
    };
    tokio::select! {
        update = agent::core::run(&credential, options, status) => install_and_relaunch(&paths, update?),
        res = tokio::signal::ctrl_c() => {
            res.context("waiting for ctrl-c")?;
            info!("shutting down");
            Ok(())
        }
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

#[cfg(windows)]
fn service(command: ServiceCommand) -> anyhow::Result<()> {
    use agent::win::service::{self, InstallOptions};
    match command {
        ServiceCommand::Install(args) => service::install(InstallOptions {
            transport: args.server.transport(),
            server: args.server.server,
            server_name: args.server.server_name,
            server_ca: args.server_ca,
            token: args.token,
            keep_credential: args.keep_credential,
            start: !args.no_start,
        }),
        ServiceCommand::Uninstall => service::uninstall(),
        ServiceCommand::Start => service::start(),
        ServiceCommand::Stop => service::stop(),
        ServiceCommand::Run => unreachable!(),
    }
}

#[cfg(windows)]
fn service_run() -> anyhow::Result<()> {
    agent::win::service::run_dispatcher()
        .context("`service run` is started by the Service Control Manager, not by hand")
}

#[cfg(windows)]
fn helper() -> anyhow::Result<()> {
    agent::win::helper::run()
}

#[cfg(windows)]
fn system_helper(service_pid: u32) -> anyhow::Result<()> {
    agent::win::system_helper::run(service_pid)
}

#[cfg(windows)]
fn capture_test(
    monitor: u32,
    seconds: u64,
    out: &std::path::Path,
    fps: u32,
    bitrate: u32,
) -> anyhow::Result<()> {
    common::logging::init();
    agent::win::capture_test::run(monitor, seconds, out, fps, bitrate)
}

#[cfg(not(windows))]
fn capture_test(_: u32, _: u64, _: &std::path::Path, _: u32, _: u32) -> anyhow::Result<()> {
    bail!("screen capture is only available on Windows")
}

#[cfg(not(windows))]
fn service(_: ServiceCommand) -> anyhow::Result<()> {
    bail!("the Windows service is only available on Windows; use `agent run` instead")
}

#[cfg(not(windows))]
fn service_run() -> anyhow::Result<()> {
    service(ServiceCommand::Run)
}

#[cfg(not(windows))]
fn helper() -> anyhow::Result<()> {
    bail!("the session helper is only available on Windows")
}

#[cfg(not(windows))]
fn system_helper(_service_pid: u32) -> anyhow::Result<()> {
    bail!("the system helper is only available on Windows")
}

#[cfg(test)]
mod tests {
    use super::server_addr;

    #[test]
    fn server_accepts_addresses_and_host_names() {
        assert_eq!(
            server_addr("192.0.2.7:4433").unwrap(),
            "192.0.2.7:4433".parse().unwrap()
        );
        assert_eq!(
            server_addr("[::1]:4433").unwrap(),
            "[::1]:4433".parse().unwrap()
        );
        // IPv4 preferred for names.
        assert_eq!(
            server_addr("localhost:4433").unwrap(),
            "127.0.0.1:4433".parse().unwrap()
        );
        assert!(server_addr("localhost").is_err(), "port required");
        assert!(server_addr("no-such-host.invalid:4433").is_err());
    }
}
