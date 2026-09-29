//! The agent's main loop, shared by console mode (`agent run`) and the
//! Windows service: keep the control connection up (heartbeats with
//! telemetry), publish connection status, and poll for signed updates.
//!
//! [`run`] only returns when a verified update is ready. Installing it is
//! left to the caller, because console mode relaunches itself while the
//! service exits and lets the service manager restart it.

use std::sync::Arc;
use std::time::Duration;

use protocol::ipc::AgentStatus;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::credstore::Credential;
use crate::telemetry::TelemetrySource;
use crate::update::{CheckOutcome, UpdateClient, VerifiedUpdate};

/// Delay before reconnecting after the connection drops.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub struct CoreOptions {
    pub heartbeat_interval: Duration,
    /// Zero disables update checks.
    pub update_interval: Duration,
    pub telemetry: Option<Arc<dyn TelemetrySource>>,
    /// Screen source for streaming (Windows service: the session helper).
    pub media: Option<Arc<crate::media::source::MediaLink>>,
    /// The user's desktop (Windows service: the session helper).
    pub desktop: Option<Arc<crate::interactive::DesktopLink>>,
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Initial status for an enrolled agent that has not connected yet.
pub fn initial_status(agent_id: Option<String>) -> AgentStatus {
    AgentStatus {
        agent_id,
        connected: false,
        version: version().to_owned(),
    }
}

/// Run until a verified update is available.
pub async fn run(
    credential: &Credential,
    options: CoreOptions,
    status: watch::Sender<AgentStatus>,
) -> anyhow::Result<VerifiedUpdate> {
    let mut config = credential.agent_config(options.heartbeat_interval)?;
    config.telemetry = options.telemetry;
    config.media = options.media;
    config.desktop = options.desktop;
    tokio::select! {
        () = connect_forever(&config, &status) => unreachable!("connect_forever never returns"),
        update = update_loop(credential, options.update_interval) => Ok(update),
    }
}

fn set_connected(status: &watch::Sender<AgentStatus>, connected: bool) {
    status.send_if_modified(|s| std::mem::replace(&mut s.connected, connected) != connected);
}

/// Connect and heartbeat, reconnecting after any failure. "Connected" means
/// the server has acknowledged a heartbeat, not merely that TLS succeeded.
async fn connect_forever(config: &crate::AgentConfig, status: &watch::Sender<AgentStatus>) {
    // Remembers when UDP is blocked, so reconnects go straight to the
    // WebSocket fallback for a while.
    let mut preference = transport::Preference::default();
    loop {
        match crate::connect_with(config, &mut preference).await {
            Ok(session) => {
                let (acks_tx, mut acks) = mpsc::unbounded_channel();
                let mark_connected = async {
                    while acks.recv().await.is_some() {
                        set_connected(status, true);
                    }
                };
                tokio::select! {
                    result = session.run(Some(acks_tx)) => {
                        if let Err(e) = result {
                            warn!("connection lost: {e}");
                        }
                    }
                    () = mark_connected => {}
                }
                session.close();
            }
            Err(e) => warn!("connect failed: {e}"),
        }
        set_connected(status, false);
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Poll for updates. Returns once a verified update is ready; never returns
/// if updates are disabled or cannot be configured.
async fn update_loop(credential: &Credential, interval: Duration) -> VerifiedUpdate {
    let Some(key) = crate::update::baked_public_key() else {
        warn!("auto-update disabled: this build has no RMM_UPDATE_PUBKEY");
        return std::future::pending().await;
    };
    if interval.is_zero() {
        info!("auto-update disabled by configuration");
        return std::future::pending().await;
    }
    let client = match UpdateClient::new(
        &credential.api_url,
        &credential.ca_pem,
        key,
        protocol::update::current_platform(),
        version().parse().expect("crate version is semver"),
    ) {
        Ok(client) => client,
        Err(e) => {
            error!("auto-update disabled: {e}");
            return std::future::pending().await;
        }
    };
    loop {
        match client.check().await {
            Ok(CheckOutcome::Available(update)) => return update,
            Ok(CheckOutcome::UpToDate) => {}
            // A bad signature is logged loudly but never installed.
            Err(e) => error!("update check failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}
