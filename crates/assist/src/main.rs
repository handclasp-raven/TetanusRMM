//! Quick assist: a one-time support session on a machine with no agent
//! installed (see `protocol::assist`).
//!
//! The user downloads this program from the server's `/assist` page, runs
//! it, accepts a warning about scams, and types the six-digit code the
//! person supporting them reads out. It then behaves as an agent for that
//! one session: the technician's viewer connects, the user is asked by name
//! whether to allow it, and closing the window ends it. Nothing is
//! installed.
//!
//! Where the server is, and which CA to trust, is appended to the
//! executable when it is downloaded. For development the same settings can
//! come from `RMM_SERVER`, `RMM_SERVER_NAME` and `RMM_SERVER_CA` (a path).
//!
//! Windows only, like the agent's screen capture. Elsewhere it builds as a
//! headless client that takes the code as its argument, connects, and
//! refuses every session (there is nobody to ask): enough to exercise a
//! server.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod session;
#[cfg(windows)]
mod win;

use protocol::assist::AssistConfig;

/// The settings appended to this executable, else the development ones
/// from the environment.
fn load_config() -> Option<AssistConfig> {
    let appended = std::env::current_exe()
        .and_then(std::fs::read)
        .ok()
        .and_then(|file| protocol::assist::read_trailer(&file));
    appended.or_else(|| {
        let server = std::env::var("RMM_SERVER").ok()?;
        let ca_pem = std::fs::read_to_string(std::env::var_os("RMM_SERVER_CA")?).ok()?;
        let server_name = std::env::var("RMM_SERVER_NAME").ok().unwrap_or_else(|| {
            server
                .rsplit_once(':')
                .map_or(server.as_str(), |(host, _)| host)
                .trim_matches(['[', ']'])
                .to_owned()
        });
        Some(AssistConfig {
            server,
            server_name,
            ca_pem,
        })
    })
}

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use session::Phase;

    common::logging::init();
    let config = load_config()
        .context("no settings: set RMM_SERVER and RMM_SERVER_CA (and RMM_SERVER_NAME)")?;
    let code = std::env::args()
        .nth(1)
        .and_then(|code| protocol::assist::normalize_code(&code))
        .context("usage: assist <six-digit code>")?;
    tokio::runtime::Runtime::new()?.block_on(async {
        tracing::info!("{}", session::status_text(Phase::Checking, false, &[]));
        let credential = session::redeem(&config, &code)
            .await
            .map_err(|e| anyhow::anyhow!(session::failure_text(&e)))?;
        let (status, mut changes) = tokio::sync::watch::channel(agent::core::initial_status(Some(
            credential.agent_id.clone(),
        )));
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let connected = changes.borrow_and_update().connected;
                tracing::info!("{}", session::status_text(Phase::Session, connected, &[]));
            }
        });
        tokio::select! {
            e = session::serve(&credential, session::Links::default(), status) => Err(e),
            res = tokio::signal::ctrl_c() => res.context("waiting for ctrl-c"),
        }
    })
}
