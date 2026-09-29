use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use common::devcerts;
use common::Identity;
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
    /// Listen for agent connections.
    Serve(ServeArgs),
    /// Generate a development CA, server certificate and agent certificate.
    GenCerts(GenCertsArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// UDP address to listen on.
    #[arg(long, env = "RMM_LISTEN", default_value = "0.0.0.0:4433")]
    listen: SocketAddr,
    /// Directory holding ca.crt, server.crt and server.key.
    #[arg(long, env = "RMM_CERTS_DIR", default_value = "dev-certs")]
    certs_dir: PathBuf,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    common::logging::init();
    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::GenCerts(args) => gen_certs(args),
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let dir = &args.certs_dir;
    let config = ServerConfig {
        listen: args.listen,
        identity: Identity::from_files(
            &dir.join(devcerts::SERVER_CERT),
            &dir.join(devcerts::SERVER_KEY),
        )
        .context("loading server certificate (run `gen-certs` first?)")?,
        client_ca: common::tls::load_certs(&dir.join(devcerts::CA_CERT))
            .context("loading client CA")?,
    };
    let server = Server::bind(config)?;

    tokio::select! {
        () = server.run() => {}
        res = tokio::signal::ctrl_c() => {
            res.context("waiting for ctrl-c")?;
            info!("shutting down");
            server.close();
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
