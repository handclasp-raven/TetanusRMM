//! Start a program on an agent's desktop, as the signed-in user (the
//! viewer's command buttons; see `protocol::launch`).
//!
//! Needs the `desktop` capability: it does nothing a technician in a
//! remote-desktop session could not do from the Run dialog. Every attempt
//! that reaches an agent is audited as `command.launch` with the command
//! and how it ended.

use std::time::Duration;

use protocol::launch::{LaunchReply, LaunchRequest};
use protocol::{read_frame, write_frame, StreamOpen, MIN_LAUNCH_VERSION};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tracing::info;

use super::RemoteError;
use crate::audit::{self, Action, NewEntry};
use crate::relay::Hub;
use crate::users::{Capability, User};

/// How long the agent gets to start the program and answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// A program that started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Launched {
    pub command: String,
    /// Who it runs as, e.g. `CORP\alice`.
    pub user: String,
}

pub async fn launch(
    pool: &PgPool,
    hub: &Hub,
    user: &User,
    agent_id: &str,
    command: &str,
) -> Result<Launched, RemoteError> {
    let request =
        LaunchRequest::new(command).map_err(|e| RemoteError::BadRequest(e.to_string()))?;
    super::authorize(pool, user, Capability::Desktop, &[agent_id.to_owned()]).await?;
    let link = super::agent(hub, agent_id)?;
    if link.version < MIN_LAUNCH_VERSION {
        return Err(RemoteError::Unsupported);
    }
    let result = exchange(&link, &request).await;
    let outcome = match &result {
        Ok(LaunchReply::Started { user }) => json!({ "result": "started", "user": user }),
        Ok(LaunchReply::NoUser) => json!({ "result": "no_user" }),
        Ok(LaunchReply::Failed(error)) => json!({ "result": "failed", "error": error }),
        Err(e) => json!({ "result": "failed", "error": e.to_string() }),
    };
    audit::append_now(
        pool,
        NewEntry::new(&user.username, Action::CommandLaunch)
            .target(agent_id)
            .detail(json!({ "command": request.command, "outcome": outcome })),
    )
    .await?;
    info!(%agent_id, user = %user.username, command = %request.command, %outcome, "launch");
    match result? {
        LaunchReply::Started { user } => Ok(Launched {
            command: request.command,
            user,
        }),
        LaunchReply::NoUser => Err(RemoteError::NoUser),
        LaunchReply::Failed(message) => Err(RemoteError::Agent {
            kind: protocol::transfer::ErrorKind::Io,
            message,
        }),
    }
}

async fn exchange(
    link: &crate::relay::AgentLink,
    request: &LaunchRequest,
) -> Result<LaunchReply, RemoteError> {
    let (mut send, mut recv) = super::open_stream(link).await?;
    write_frame(&mut send, &StreamOpen::Launch(request.clone())).await?;
    send.shutdown()
        .await
        .map_err(|e| RemoteError::Transport(e.to_string()))?;
    match tokio::time::timeout(REPLY_TIMEOUT, read_frame::<_, LaunchReply>(&mut recv)).await {
        Ok(Ok(Some(reply))) => Ok(reply),
        Ok(Ok(None)) => Err(RemoteError::Transport(
            "the agent closed the stream without answering".into(),
        )),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(RemoteError::Transport(
            "no answer from the agent in time".into(),
        )),
    }
}
