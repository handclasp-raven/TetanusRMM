//! The agent's main loop, shared by console mode (`agent run`) and the
//! Windows service: keep the control connection up (heartbeats with
//! telemetry), publish connection status, and poll for signed updates.
//!
//! [`run`] only returns when a verified update is ready. Installing it is
//! left to the caller, because console mode relaunches itself while the
//! service exits and lets the service manager restart it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use ed25519_dalek::VerifyingKey;
use protocol::ipc::AgentStatus;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::credstore::{Credential, CredentialStore};
use crate::telemetry::TelemetrySource;
use crate::update::{CheckOutcome, UpdateClient, VerifiedUpdate};

/// Delay before reconnecting after the connection drops.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub struct CoreOptions {
    pub heartbeat_interval: Duration,
    /// Zero disables update checks.
    pub update_interval: Duration,
    /// Where the credential is kept, to pin the update key in. Without it,
    /// only a build with a baked-in key updates itself.
    pub store: Option<CredentialStore>,
    pub telemetry: Option<Arc<dyn TelemetrySource>>,
    /// Screen source for streaming (Windows service: the session helper).
    pub media: Option<Arc<crate::media::source::MediaLink>>,
    /// The user's desktop (Windows service: the session helper).
    pub desktop: Option<Arc<crate::interactive::DesktopLink>>,
    /// Which remote operations to serve.
    pub remote: crate::remote::Allowed,
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
    config.remote = options.remote;
    tokio::select! {
        () = connect_forever(&config, &status) => unreachable!("connect_forever never returns"),
        update = update_loop(credential, options.update_interval, options.store.as_ref()) => Ok(update),
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

/// The key updates must be signed with: the one baked into this build, else
/// the one pinned in the credential, else the one the server publishes,
/// which is then pinned in `store` so that it is asked for only once. The
/// request is verified against the credential's CA, like enrollment was.
/// `None` if the server has no key (or there is nowhere to pin one).
pub async fn update_key(
    credential: &Credential,
    store: Option<&CredentialStore>,
) -> anyhow::Result<Option<VerifyingKey>> {
    if let Some(key) = crate::update::baked_public_key() {
        return Ok(Some(key));
    }
    if let Some(pinned) = &credential.update_pubkey {
        let key = crate::update::parse_public_key(pinned)
            .context("the pinned update key is malformed")?;
        return Ok(Some(key));
    }
    let Some(store) = store else {
        return Ok(None);
    };
    let Some(fetched) =
        crate::update::fetch_public_key(&credential.api_url, &credential.ca_pem).await?
    else {
        return Ok(None);
    };
    let key = crate::update::parse_public_key(&fetched).context("malformed update key")?;
    let mut pinned = credential.clone();
    pinned.update_pubkey = Some(fetched.clone());
    store.save(&pinned).context("pinning the update key")?;
    info!(key = %fetched, "pinned the server's update key");
    Ok(Some(key))
}

/// Poll for updates. Returns once a verified update is ready; never returns
/// if updates are disabled or cannot be configured.
async fn update_loop(
    credential: &Credential,
    interval: Duration,
    store: Option<&CredentialStore>,
) -> VerifiedUpdate {
    if interval.is_zero() {
        info!("auto-update disabled by configuration");
        return std::future::pending().await;
    }
    let key = loop {
        match update_key(credential, store).await {
            Ok(Some(key)) => break key,
            Ok(None) => debug!("no update key yet: the server has not published one"),
            Err(e) => warn!("could not get the update key: {e:#}"),
        }
        tokio::time::sleep(interval).await;
    };
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
