use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use common::devcerts;
use server::api::{self, AppState};
use server::auth::AuthSettings;
use server::config::ServeConfig;
use server::users::{self, Role};
use server::{Server, ServerConfig};
use tracing::info;

#[derive(Parser)]
#[command(about = "RMM central server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the QUIC agent listener and the HTTPS API.
    Serve(ServeConfig),
    /// Generate a development CA, server certificate and agent certificate.
    GenCerts(GenCertsArgs),
    /// Create a user. Reads the password from the first line of stdin and
    /// prints the TOTP secret to enrol in an authenticator app.
    CreateUser(CreateUserArgs),
}

#[derive(Args)]
struct GenCertsArgs {
    /// Output directory.
    #[arg(long, default_value = "dev-certs")]
    out: PathBuf,
    /// Common name for the generated agent certificate.
    #[arg(long, default_value = "dev-agent-1")]
    agent_id: String,
    /// Replace existing certificates in the output directory.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct CreateUserArgs {
    #[arg(long)]
    username: String,
    #[arg(long, value_enum)]
    role: Role,
    /// Postgres connection URL.
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    database_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    common::logging::init();
    match Cli::parse().command {
        Command::Serve(config) => serve(config).await,
        Command::GenCerts(args) => gen_certs(args),
        Command::CreateUser(args) => create_user(args).await,
    }
}

async fn serve(config: ServeConfig) -> anyhow::Result<()> {
    let pool = server::db::connect(&config.database_url, config.db_max_connections).await?;
    info!("database connected and migrated");

    let quic = Server::bind(ServerConfig {
        listen: config.quic_listen,
        identity: config
            .quic_identity()
            .context("loading server certificate (run `gen-certs` first?)")?,
        client_ca: config.agent_ca().context("loading agent CA")?,
    })?
    .with_registry(pool.clone());

    let tls = common::tls::https_server_config(
        &config.api_identity().context("loading API certificate")?,
    )?;
    let app = api::router(AppState {
        pool,
        auth: AuthSettings {
            session_ttl: config.session_ttl(),
        },
    });
    let listener = std::net::TcpListener::bind(config.api_listen)
        .with_context(|| format!("binding API listener on {}", config.api_listen))?;
    info!(addr = %config.api_listen, "HTTPS API listening");
    let handle = axum_server::Handle::new();

    tokio::select! {
        () = quic.run() => bail!("QUIC listener stopped"),
        res = api::serve(listener, tls, app, handle.clone()) => {
            res.context("HTTPS API stopped")?;
            bail!("HTTPS API stopped");
        }
        res = tokio::signal::ctrl_c() => {
            res.context("waiting for ctrl-c")?;
            info!("shutting down");
            quic.close();
            handle.shutdown();
        }
    }
    Ok(())
}

fn gen_certs(args: GenCertsArgs) -> anyhow::Result<()> {
    if args.out.join(devcerts::CA_CERT).exists() && !args.force {
        bail!(
            "{} already contains certificates; pass --force to replace them",
            args.out.display()
        );
    }
    let certs = devcerts::generate(&args.agent_id)?;
    certs.write_to_dir(&args.out)?;
    info!(dir = %args.out.display(), agent_id = %args.agent_id, "wrote dev certificates");
    Ok(())
}

async fn create_user(args: CreateUserArgs) -> anyhow::Result<()> {
    let mut password = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut password)
        .context("reading password from stdin")?;
    let password = password.trim_end_matches(['\r', '\n']);

    let pool = server::db::connect(&args.database_url, 1).await?;
    let created = users::create_user(&pool, "cli", &args.username, password, args.role).await?;
    info!(username = %created.user.username, role = ?created.user.role, "user created");

    // This is command output for the operator, not logging: the secret must
    // not end up in log files.
    let mut out = std::io::stdout().lock();
    writeln!(out, "TOTP secret: {}", created.totp_secret)?;
    writeln!(out, "otpauth URL: {}", created.otpauth_url)?;
    Ok(())
}
