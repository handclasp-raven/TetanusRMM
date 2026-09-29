//! Interactive shell: a WebSocket from the client, relayed onto a stream to
//! the agent (see `protocol::shell` for both legs).
//!
//! [`open`] starts the shell on the agent before the WebSocket upgrade, so
//! a failure (agent offline, no ConPTY) is an ordinary HTTP error. [`relay`]
//! then copies bytes both ways until the shell exits, the client closes, or
//! the agent goes away. Audited as `shell.open` (whether it started) and
//! `shell.close` (duration, bytes each way, exit code, who ended it); the
//! `shell_id` in both is the `shell.open` row's id. Keystrokes and output
//! themselves are not recorded.

use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message as WsMessage, WebSocket};
use protocol::shell::{data_frames, parse_client_control, ClientFrame, ShellOutput, TermSize};
use protocol::{read_frame, write_frame, StreamOpen};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::RemoteError;
use crate::audit::{self, Action, NewEntry};
use crate::relay::Hub;
use crate::users::{Capability, User};

/// How long the agent has to start the shell.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// WebSocket close code for a malformed control message (policy violation).
const CLOSE_PROTOCOL: u16 = 1008;

/// A shell running on an agent, not yet attached to a client.
pub struct OpenShell {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    audit: CloseAudit,
    /// Counts this shell in the agent list until the relay ends.
    active: crate::relay::ActiveShell,
}

/// Authorize, then start a shell of `size` on the agent.
pub async fn open(
    pool: &PgPool,
    hub: &Hub,
    user: &User,
    agent_id: &str,
    size: TermSize,
) -> Result<OpenShell, RemoteError> {
    super::authorize(pool, user, Capability::Shell, &[agent_id.to_owned()]).await?;
    let link = super::agent(hub, agent_id)?;
    let started = start(&link, size).await;
    let mut detail = json!({ "cols": size.cols, "rows": size.rows, "started": started.is_ok() });
    if let Err(e) = &started {
        detail["error"] = e.to_string().into();
    }
    let entry = audit::append_now(
        pool,
        NewEntry::new(&user.username, Action::ShellOpen)
            .target(agent_id)
            .detail(detail),
    )
    .await?;
    let (send, recv) = started?;
    info!(shell_id = entry.id, user = %user.username, agent_id, "shell started");
    Ok(OpenShell {
        send,
        recv,
        active: link.shell_started(),
        audit: CloseAudit {
            pool: pool.clone(),
            user: user.username.clone(),
            agent_id: agent_id.to_owned(),
            shell_id: entry.id,
            started: Instant::now(),
            bytes_in: 0,
            bytes_out: 0,
            exit_code: None,
            ended_by: "upgrade_failed",
        },
    })
}

async fn start(
    link: &crate::relay::AgentLink,
    size: TermSize,
) -> Result<(quinn::SendStream, quinn::RecvStream), RemoteError> {
    let (mut send, mut recv) = super::open_stream(link).await?;
    write_frame(&mut send, &StreamOpen::Shell { size }).await?;
    let first = tokio::time::timeout(START_TIMEOUT, read_frame::<_, ShellOutput>(&mut recv))
        .await
        .map_err(|_| {
            RemoteError::Transport("the agent did not start the shell in time".into())
        })??;
    match first {
        Some(ShellOutput::Started) => Ok((send, recv)),
        Some(ShellOutput::Error(message)) => Err(RemoteError::Agent {
            kind: protocol::transfer::ErrorKind::Io,
            message,
        }),
        Some(_) => Err(RemoteError::Transport(
            "unexpected message from the agent".into(),
        )),
        None => Err(RemoteError::Transport("the agent closed the stream".into())),
    }
}

fn ws_message(frame: ClientFrame) -> WsMessage {
    match frame {
        ClientFrame::Binary(bytes) => WsMessage::Binary(bytes.into()),
        ClientFrame::Text(text) => WsMessage::Text(text.into()),
    }
}

/// Relay between the client's WebSocket and the agent's shell until either
/// side ends.
pub async fn relay(mut socket: WebSocket, shell: OpenShell) {
    let OpenShell {
        mut send,
        mut recv,
        mut audit,
        active: _active,
    } = shell;
    audit.ended_by = "agent";
    let _ = socket
        .send(ws_message(ShellOutput::Started.into_client_frame().0))
        .await;

    // Frames from the agent, read in their own task (read_frame is not
    // cancel-safe, so it cannot race in the select below).
    let (output_tx, mut output) = mpsc::channel::<ShellOutput>(32);
    let reader = tokio::spawn(async move {
        loop {
            match read_frame::<_, ShellOutput>(&mut recv).await {
                Ok(Some(msg)) => {
                    if output_tx.send(msg).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    warn!("shell stream from agent failed: {e}");
                    return;
                }
            }
        }
    });

    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    audit.bytes_in += bytes.len() as u64;
                    let mut ok = true;
                    for frame in data_frames(&bytes) {
                        ok &= write_frame(&mut send, &frame).await.is_ok();
                    }
                    if !ok {
                        break;
                    }
                }
                Some(Ok(WsMessage::Text(text))) => match parse_client_control(text.as_str()) {
                    Ok(input) => {
                        if write_frame(&mut send, &input).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(shell_id = audit.shell_id, "bad control message: {e}");
                        audit.ended_by = "protocol_error";
                        let _ = socket
                            .send(WsMessage::Close(Some(CloseFrame {
                                code: CLOSE_PROTOCOL,
                                reason: e.to_string().into(),
                            })))
                            .await;
                        break;
                    }
                },
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => {}
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => {
                    audit.ended_by = "client";
                    break;
                }
            },
            msg = output.recv() => {
                let Some(msg) = msg else {
                    // The agent went away without reporting an exit.
                    let lost = ShellOutput::Error("the connection to the agent was lost".into());
                    let _ = socket.send(ws_message(lost.into_client_frame().0)).await;
                    break;
                };
                match &msg {
                    ShellOutput::Data(bytes) => audit.bytes_out += bytes.len() as u64,
                    ShellOutput::Exited { code } => {
                        audit.exit_code = *code;
                        audit.ended_by = "exit";
                    }
                    _ => {}
                }
                let (frame, last) = msg.into_client_frame();
                let sent = socket.send(ws_message(frame)).await;
                if last {
                    let _ = socket.send(WsMessage::Close(None)).await;
                    break;
                }
                if sent.is_err() {
                    audit.ended_by = "client";
                    break;
                }
            }
        }
    }
    // Hang up: the agent kills the shell if it is still running.
    let _ = send.finish();
    reader.abort();
}

/// Audits the end of a shell when dropped, however it ended (including a
/// WebSocket upgrade that never completed).
struct CloseAudit {
    pool: PgPool,
    user: String,
    agent_id: String,
    shell_id: i64,
    started: Instant,
    bytes_in: u64,
    bytes_out: u64,
    exit_code: Option<i32>,
    /// `exit`, `client`, `agent`, `protocol_error` or `upgrade_failed`.
    ended_by: &'static str,
}

impl Drop for CloseAudit {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let entry = NewEntry::new(std::mem::take(&mut self.user), Action::ShellClose)
            .target(std::mem::take(&mut self.agent_id))
            .detail(json!({
                "shell_id": self.shell_id,
                "duration_secs": self.started.elapsed().as_secs_f64(),
                "bytes_in": self.bytes_in,
                "bytes_out": self.bytes_out,
                "exit_code": self.exit_code,
                "ended_by": self.ended_by,
            }));
        info!(
            shell_id = self.shell_id,
            ended_by = self.ended_by,
            "shell ended"
        );
        let pool = self.pool.clone();
        runtime.spawn(async move {
            if let Err(e) = audit::append_now(&pool, entry).await {
                warn!("auditing a shell's end failed: {e}");
            }
        });
    }
}
