//! Remote operations on agents, driven through the HTTPS API with no viewer
//! involved:
//!
//! - [`shell`]: interactive PowerShell, relayed between a WebSocket and a
//!   stream to the agent
//! - [`script`]: run a script on one or many agents and collect each result
//! - [`files`]: upload and download files, streamed through in chunks
//!
//! The server opens one bidirectional QUIC stream per operation on the
//! agent's existing connection (agents are behind NAT; they never accept
//! connections). Each operation checks the user's role first; a refusal is
//! audited as `permission.denied`. Every shell, script run and transfer is
//! audited too, with who, which agent(s) and what happened.

pub mod files;
pub mod script;
pub mod shell;

use std::sync::Arc;

use protocol::transfer::ErrorKind;
use protocol::MIN_REMOTE_OPS_VERSION;
use serde_json::json;
use sqlx::PgPool;

use crate::audit::{self, Action, NewEntry};
use crate::relay::{AgentLink, Hub};
use crate::users::{Capability, User};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The user's role does not allow this (already audited).
    #[error("forbidden")]
    Forbidden,
    #[error("agent is not connected")]
    AgentOffline,
    #[error("agent version does not support remote operations; update it")]
    Unsupported,
    #[error("{0}")]
    BadRequest(String),
    /// The agent refused or failed the operation.
    #[error("{message}")]
    Agent { kind: ErrorKind, message: String },
    /// The stream to the agent failed, or it answered out of turn.
    #[error("agent connection failed: {0}")]
    Transport(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<quinn::ConnectionError> for RemoteError {
    fn from(e: quinn::ConnectionError) -> Self {
        RemoteError::Transport(e.to_string())
    }
}

impl From<protocol::FrameError> for RemoteError {
    fn from(e: protocol::FrameError) -> Self {
        RemoteError::Transport(e.to_string())
    }
}

/// Check that `user` may use `capability` on `agents`. A refusal is audited
/// as `permission.denied` and returned as [`RemoteError::Forbidden`].
pub async fn authorize(
    pool: &PgPool,
    user: &User,
    capability: Capability,
    agents: &[String],
) -> Result<(), RemoteError> {
    if user.role.can(capability) {
        return Ok(());
    }
    let mut entry = NewEntry::new(&user.username, Action::PermissionDenied).detail(json!({
        "capability": capability,
        "role": user.role,
        "agents": agents,
    }));
    if let [agent] = agents {
        entry = entry.target(agent);
    }
    audit::append_now(pool, entry).await?;
    Err(RemoteError::Forbidden)
}

/// The connected agent `agent_id`, if it can serve remote operations.
pub fn agent(hub: &Hub, agent_id: &str) -> Result<Arc<AgentLink>, RemoteError> {
    let link = hub.get(agent_id).ok_or(RemoteError::AgentOffline)?;
    if link.version < MIN_REMOTE_OPS_VERSION {
        return Err(RemoteError::Unsupported);
    }
    Ok(link)
}

/// A new stream to the agent.
pub async fn open_stream(
    link: &AgentLink,
) -> Result<(quinn::SendStream, quinn::RecvStream), RemoteError> {
    link.open_stream()
        .await
        .ok_or(RemoteError::AgentOffline)?
        .map_err(Into::into)
}
