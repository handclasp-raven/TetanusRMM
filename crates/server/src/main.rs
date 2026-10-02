use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use common::devcerts;
use server::api::{self, AppState};
use server::auth::AuthSettings;
use server::config::ServeConfig;
use server::updates;
use server::users::{self, Role};
use server::{Registry, Server, ServerConfig};
use tracing::{info, warn};

#[derive(Parser)]
#[command(about = "RMM central server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the QUIC agent listener and the HTTPS API.
    Serve(Box<ServeConfig>),
    /// Generate a development CA (which also signs agent certificates at
    /// enrollment) and a server certificate.
    GenCerts(GenCertsArgs),
    /// Create a user. Reads the password from the first line of stdin and
    /// prints the TOTP secret to enrol in an authenticator app.
    CreateUser(CreateUserArgs),
    /// Generate an ed25519 key pair for signing agent updates.
    GenUpdateKey(GenUpdateKeyArgs),
    /// Write a detached signature `<file>.sig` for an agent build.
    SignUpdate(SignUpdateArgs),
    /// Copy a signed agent build into the updates directory and publish it.
    PublishUpdate(PublishUpdateArgs),
    /// Copy a viewer build into the updates directory, for the support TUI
    /// to download.
    PublishViewer(PublishViewerArgs),
    /// Copy the support TUI's wheel into the updates directory, for the
    /// install page (`/install`) to offer.
    PublishTui(PublishTuiArgs),
}

#[derive(Args)]
struct GenUpdateKeyArgs {
    /// Output directory for update.key (secret) and update.pub.
    #[arg(long, default_value = "update-keys")]
    out: PathBuf,
    /// Replace an existing key.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct Release {
    /// Target platform, e.g. windows-x86_64 or linux-x86_64.
    #[arg(long)]
    platform: String,
    /// Semantic version of this build, e.g. 0.2.0.
    #[arg(long)]
    version: String,
}

#[derive(Args)]
struct SignUpdateArgs {
    /// Agent binary to sign.
    file: PathBuf,
    #[command(flatten)]
    release: Release,
    /// Signing key from `gen-update-key`.
    #[arg(long, default_value = "update-keys/update.key")]
    key: PathBuf,
}

#[derive(Args)]
struct PublishUpdateArgs {
    /// Signed agent binary; `<file>.sig` must exist.
    file: PathBuf,
    #[command(flatten)]
    release: Release,
    #[arg(long, env = "RMM_UPDATES_DIR", default_value = "updates")]
    updates_dir: PathBuf,
    /// Public key from `gen-update-key`, published for agents built without
    /// one to pin.
    #[arg(long, default_value = "update-keys/update.pub")]
    pubkey: PathBuf,
}

#[derive(Args)]
struct PublishViewerArgs {
    /// Viewer binary.
    file: PathBuf,
    #[command(flatten)]
    release: Release,
    #[arg(long, env = "RMM_UPDATES_DIR", default_value = "updates")]
    updates_dir: PathBuf,
}

#[derive(Args)]
struct PublishTuiArgs {
    /// The wheel, e.g. `rmm_tui-0.1.0-py3-none-any.whl`.
    file: PathBuf,
    #[arg(long, env = "RMM_UPDATES_DIR", default_value = "updates")]
    updates_dir: PathBuf,
}

#[derive(Args)]
struct GenCertsArgs {
    /// Output directory.
    #[arg(long, default_value = "dev-certs")]
    out: PathBuf,
    /// Replace existing certificates in the output directory.
    #[arg(long)]
    force: bool,
    /// Extra DNS name or IP address for the server certificate, besides
    /// localhost/127.0.0.1/::1. Repeatable.
    #[arg(long = "san")]
    extra_names: Vec<String>,
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
        Command::Serve(config) => serve(*config).await,
        Command::GenCerts(args) => gen_certs(args),
        Command::CreateUser(args) => create_user(args).await,
        Command::GenUpdateKey(args) => gen_update_key(args),
        Command::SignUpdate(args) => {
            let sig = updates::sign_file(
                &args.key,
                &args.file,
                &args.release.platform,
                &args.release.version,
            )?;
            info!(signature = %sig.display(), "signed");
            Ok(())
        }
        Command::PublishUpdate(args) => {
            if args.pubkey.exists() {
                updates::publish_public_key(&args.updates_dir, &args.pubkey)?;
            } else {
                warn!(
                    pubkey = %args.pubkey.display(),
                    "no public key to publish: only agents built with RMM_UPDATE_PUBKEY will update"
                );
            }
            let manifest = updates::publish(
                &args.updates_dir,
                &args.file,
                &args.release.platform,
                &args.release.version,
            )?;
            info!(platform = %manifest.platform, version = %manifest.version, sha256 = %manifest.sha256, "published");
            Ok(())
        }
        Command::PublishViewer(args) => {
            let manifest = updates::publish_viewer(
                &args.updates_dir,
                &args.file,
                &args.release.platform,
                &args.release.version,
            )?;
            info!(platform = %manifest.platform, version = %manifest.version, sha256 = %manifest.sha256, "published viewer");
            Ok(())
        }
        Command::PublishTui(args) => {
            let name = updates::publish_tui(&args.updates_dir, &args.file)?;
            info!(wheel = %name, "published TUI");
            Ok(())
        }
    }
}

async fn serve(config: ServeConfig) -> anyhow::Result<()> {
    let pool = server::db::connect(&config.database_url, config.db_max_connections).await?;
    info!("database connected and migrated");

    let enrollment_ca = config
        .enrollment_ca()
        .context("loading CA key for enrollment (re-run `gen-certs --force`?)")?;
    let server_ca_pem = enrollment_ca.ca_pem().to_owned();
    // What the support TUI asks its user to check on first sign-in.
    info!(sha256 = %server::quic::pem_fingerprint(&server_ca_pem)?, "CA certificate fingerprint");
    let quic = Server::bind(ServerConfig {
        listen: config.quic_listen,
        ws_listen: config.ws_listen(),
        identity: config
            .quic_identity()
            .context("loading server certificate (run `gen-certs` first?)")?,
        client_ca: config.agent_ca().context("loading agent CA")?,
    })?
    .with_registry(Registry {
        pool: pool.clone(),
        ca: enrollment_ca,
        api_url: config.public_url(),
    });

    let stun = if config.no_direct {
        info!("direct paths disabled: every session stays on the relay");
        None
    } else {
        let socket = server::stun::bind(config.stun_listen)
            .await
            .with_context(|| format!("binding STUN responder on {}", config.stun_listen))?;
        Some(socket)
    };
    let stun_port = stun.as_ref().map(|socket| {
        config
            .stun_announce_port
            .or_else(|| socket.local_addr().ok().map(|a| a.port()))
            .unwrap_or(config.stun_listen.port())
    });
    let quic = quic.with_direct_paths(!config.no_direct, stun_port);
    let stun = async {
        match stun {
            Some(socket) => server::stun::serve(socket).await,
            None => std::future::pending().await,
        }
    };

    let tls = common::tls::https_server_config(
        &config.api_identity().context("loading API certificate")?,
    )?;
    let metrics = match config.metrics_listen {
        Some(addr) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding metrics listener on {addr}"))?;
            info!(%addr, "Prometheus metrics at /metrics");
            Some(server::metrics::serve(listener, pool.clone()))
        }
        None => None,
    };
    let metrics = async {
        match metrics {
            Some(serving) => serving.await,
            None => std::future::pending().await,
        }
    };
    let app = api::router(AppState {
        pool,
        auth: AuthSettings {
            session_ttl: config.session_ttl(),
        },
        public_url: config.public_url(),
        updates_dir: config.updates_dir.clone(),
        hub: Some(quic.hub()),
        server_ca_pem,
    });
    let listener = std::net::TcpListener::bind(config.api_listen)
        .with_context(|| format!("binding API listener on {}", config.api_listen))?;
    info!(addr = %config.api_listen, "HTTPS API listening");
    let handle = axum_server::Handle::new();

    tokio::select! {
        () = quic.run() => bail!("QUIC listener stopped"),
        () = stun => bail!("STUN responder stopped"),
        res = metrics => {
            res.context("metrics listener stopped")?;
            bail!("metrics listener stopped");
        }
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
    let certs = devcerts::generate_with_names("unused", &args.extra_names)?;
    certs.write_to_dir(&args.out)?;
    info!(dir = %args.out.display(), extra_names = ?args.extra_names, "wrote dev CA and server certificate");
    // Command output, not logging: staff compare it with what the TUI shows.
    writeln!(
        std::io::stdout().lock(),
        "CA fingerprint (SHA-256): {}",
        server::quic::pem_fingerprint(&certs.ca_cert)?
    )?;
    Ok(())
}

fn gen_update_key(args: GenUpdateKeyArgs) -> anyhow::Result<()> {
    if args.out.join(updates::SIGNING_KEY_FILE).exists() && !args.force {
        bail!(
            "{} already contains an update key; pass --force to replace it \
             (agents built with the old public key will reject updates signed by the new one)",
            args.out.display()
        );
    }
    let key = updates::generate_key();
    updates::write_key_pair(&args.out, &key)?;
    info!(dir = %args.out.display(), "wrote update signing key");
    // Command output, not logging: the operator needs this to build agents.
    writeln!(
        std::io::stdout().lock(),
        "RMM_UPDATE_PUBKEY={}",
        updates::public_key_hex(&key.verifying_key())
    )?;
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
