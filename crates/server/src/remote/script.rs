//! Run a script on one or more agents and collect every result.
//!
//! Each agent gets its own stream, all at once; the call returns when every
//! agent has answered, timed out, or turned out to be unreachable. Offline
//! agents do not fail the run, they are reported per agent. The run is
//! audited twice: `script.run` before anything is sent (who, which agents,
//! the script's first line, size and SHA-256) and `script.complete` after
//! (each agent's status and exit code). The id of the first row is the
//! run's `run_id`.

use std::collections::HashSet;
use std::time::Duration;

use futures_util::future::join_all;
use protocol::script::{ScriptReply, ScriptRequest};
use protocol::{read_frame, write_frame, StreamOpen};
use serde::Serialize;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::info;

use super::RemoteError;
use crate::audit::{self, Action, NewEntry};
use crate::relay::Hub;
use crate::users::{Capability, User};

/// Most agents one run may target.
pub const MAX_AGENTS: usize = 500;

/// Characters of the script kept in the audit log.
const AUDIT_SUMMARY_CHARS: usize = 200;

/// How long past the script's own timeout to wait for the agent's answer
/// (starting the interpreter, killing the process tree, the network).
const REPLY_GRACE: Duration = Duration::from_secs(30);

/// How one agent's part of a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    Reply(ScriptReply),
    Offline,
    Unsupported,
    /// The stream failed or the agent never answered.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Ran to the end; see `exit_code`.
    Completed,
    /// Killed at the timeout.
    TimedOut,
    Offline,
    /// The agent is too old to run scripts.
    Unsupported,
    /// Could not be run, or no answer; see `error`.
    Failed,
}

/// One agent's result, as returned by the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentResult {
    pub agent_id: String,
    pub status: Status,
    pub exit_code: Option<i32>,
    /// Output decoded as UTF-8 (invalid bytes replaced).
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

impl AgentResult {
    fn new(agent_id: String, outcome: AgentOutcome) -> Self {
        let mut r = AgentResult {
            agent_id,
            status: Status::Failed,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: None,
            error: None,
        };
        match outcome {
            AgentOutcome::Reply(ScriptReply::Completed(result)) => {
                r.status = if result.timed_out {
                    Status::TimedOut
                } else {
                    Status::Completed
                };
                r.exit_code = result.exit_code;
                r.stdout = String::from_utf8_lossy(&result.stdout).into_owned();
                r.stderr = String::from_utf8_lossy(&result.stderr).into_owned();
                r.stdout_truncated = result.stdout_truncated;
                r.stderr_truncated = result.stderr_truncated;
                r.duration_ms = Some(result.duration_ms);
            }
            AgentOutcome::Reply(ScriptReply::Failed(error)) | AgentOutcome::Error(error) => {
                r.error = Some(error);
            }
            AgentOutcome::Offline => {
                r.status = Status::Offline;
                r.error = Some("agent is not connected".into());
            }
            AgentOutcome::Unsupported => {
                r.status = Status::Unsupported;
                r.error = Some(RemoteError::Unsupported.to_string());
            }
        }
        r
    }

    fn succeeded(&self) -> bool {
        self.status == Status::Completed && self.exit_code == Some(0)
    }

    fn ran(&self) -> bool {
        matches!(self.status, Status::Completed | Status::TimedOut)
    }
}

/// Counts over a run's results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub total: usize,
    /// Completed with exit code 0.
    pub succeeded: usize,
    /// Ran, but exited non-zero or timed out.
    pub failed: usize,
    /// Never ran: offline, unsupported, or could not be started.
    pub not_run: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    pub run_id: i64,
    pub summary: Summary,
    /// In the order the agents were requested.
    pub results: Vec<AgentResult>,
}

/// Validate the requested agent ids: trimmed, non-empty, at most
/// [`MAX_AGENTS`], duplicates dropped (first occurrence kept).
pub fn normalize_agents(agent_ids: &[String]) -> Result<Vec<String>, RemoteError> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for id in agent_ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(RemoteError::BadRequest(
                "agent ids must not be empty".into(),
            ));
        }
        if seen.insert(id) {
            out.push(id.to_owned());
        }
    }
    if out.is_empty() {
        return Err(RemoteError::BadRequest("no agents given".into()));
    }
    if out.len() > MAX_AGENTS {
        return Err(RemoteError::BadRequest(format!(
            "at most {MAX_AGENTS} agents per run"
        )));
    }
    Ok(out)
}

/// Combine per-agent outcomes into results in `requested` order. An agent
/// with no outcome is reported as failed; outcomes for agents that were not
/// requested are ignored.
pub fn aggregate(
    requested: &[String],
    outcomes: Vec<(String, AgentOutcome)>,
) -> (Vec<AgentResult>, Summary) {
    let mut outcomes: std::collections::HashMap<String, AgentOutcome> =
        outcomes.into_iter().collect();
    let results: Vec<AgentResult> = requested
        .iter()
        .map(|id| {
            let outcome = outcomes
                .remove(id)
                .unwrap_or_else(|| AgentOutcome::Error("no result".into()));
            AgentResult::new(id.clone(), outcome)
        })
        .collect();
    let succeeded = results.iter().filter(|r| r.succeeded()).count();
    let ran = results.iter().filter(|r| r.ran()).count();
    let summary = Summary {
        total: results.len(),
        succeeded,
        failed: ran - succeeded,
        not_run: results.len() - ran,
    };
    (results, summary)
}

/// `{agent_id: {status, exit_code}}` for the `script.complete` audit row.
pub fn audit_results(results: &[AgentResult]) -> Value {
    let map: Map<String, Value> = results
        .iter()
        .map(|r| {
            (
                r.agent_id.clone(),
                json!({ "status": r.status, "exit_code": r.exit_code }),
            )
        })
        .collect();
    Value::Object(map)
}

/// Run `request` on every agent in `agent_ids` (already normalized) and
/// wait for all of them.
pub async fn run(
    pool: &PgPool,
    hub: &Hub,
    user: &User,
    agent_ids: Vec<String>,
    request: ScriptRequest,
) -> Result<RunReport, RemoteError> {
    super::authorize(pool, user, Capability::Script, &agent_ids).await?;
    let started = audit::append_now(
        pool,
        NewEntry::new(&user.username, Action::ScriptRun).detail(json!({
            "agents": agent_ids,
            "summary": request.summary(AUDIT_SUMMARY_CHARS),
            "bytes": request.script.len(),
            "sha256": hex::encode(protocol::transfer::sha256(request.script.as_bytes())),
            "timeout_secs": request.timeout_secs,
        })),
    )
    .await?;
    let run_id = started.id;
    info!(run_id, user = %user.username, agents = agent_ids.len(), "script run started");

    let outcomes = join_all(agent_ids.iter().map(|id| {
        let request = &request;
        async move { (id.clone(), run_on(hub, id, request).await) }
    }))
    .await;
    let (results, summary) = aggregate(&agent_ids, outcomes);

    audit::append_now(
        pool,
        NewEntry::new(&user.username, Action::ScriptComplete).detail(json!({
            "run_id": run_id,
            "summary": summary,
            "results": audit_results(&results),
        })),
    )
    .await?;
    info!(run_id, ?summary, "script run finished");
    Ok(RunReport {
        run_id,
        summary,
        results,
    })
}

async fn run_on(hub: &Hub, agent_id: &str, request: &ScriptRequest) -> AgentOutcome {
    let link = match super::agent(hub, agent_id) {
        Ok(link) => link,
        Err(RemoteError::Unsupported) => return AgentOutcome::Unsupported,
        Err(_) => return AgentOutcome::Offline,
    };
    match super::open_stream(&link).await {
        Ok((mut send, mut recv)) => exchange(&mut send, &mut recv, request).await,
        Err(e) => AgentOutcome::Error(e.to_string()),
    }
}

/// Send the script on a fresh stream and wait for the single reply.
pub async fn exchange<W, R>(send: &mut W, recv: &mut R, request: &ScriptRequest) -> AgentOutcome
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let sent = async {
        write_frame(send, &StreamOpen::Script(request.clone())).await?;
        send.shutdown().await?;
        Ok::<(), protocol::FrameError>(())
    };
    if let Err(e) = sent.await {
        return AgentOutcome::Error(format!("sending the script: {e}"));
    }
    let wait = Duration::from_secs(request.timeout_secs.into()) + REPLY_GRACE;
    match tokio::time::timeout(wait, read_frame::<_, ScriptReply>(recv)).await {
        Ok(Ok(Some(reply))) => AgentOutcome::Reply(reply),
        Ok(Ok(None)) => AgentOutcome::Error("the agent closed the stream without a result".into()),
        Ok(Err(e)) => AgentOutcome::Error(format!("reading the result: {e}")),
        Err(_) => AgentOutcome::Error("no result from the agent in time".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::script::ScriptResult;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn completed(exit_code: Option<i32>, timed_out: bool, stdout: &[u8]) -> AgentOutcome {
        AgentOutcome::Reply(ScriptReply::Completed(ScriptResult {
            exit_code,
            stdout: stdout.to_vec(),
            stderr: b"warn".to_vec(),
            stdout_truncated: false,
            stderr_truncated: true,
            timed_out,
            duration_ms: 12,
        }))
    }

    #[test]
    fn agents_are_trimmed_deduplicated_and_bounded() {
        assert_eq!(
            normalize_agents(&ids(&["a", " b ", "a", "c", "b"])).unwrap(),
            ids(&["a", "b", "c"])
        );
        assert!(matches!(
            normalize_agents(&[]),
            Err(RemoteError::BadRequest(_))
        ));
        assert!(matches!(
            normalize_agents(&ids(&["a", "  "])),
            Err(RemoteError::BadRequest(_))
        ));
        let many: Vec<String> = (0..=MAX_AGENTS).map(|i| format!("agt-{i}")).collect();
        assert!(normalize_agents(&many).is_err());
        assert!(normalize_agents(&many[..MAX_AGENTS]).is_ok());
    }

    #[test]
    fn results_keep_request_order_and_summarize_every_kind_of_ending() {
        let requested = ids(&["ok", "nonzero", "slow", "gone", "old", "broken", "silent"]);
        // Arrival order differs from request order; "silent" never answers
        // and "stranger" was never asked.
        let outcomes = vec![
            (
                "broken".into(),
                AgentOutcome::Reply(ScriptReply::Failed("no powershell".into())),
            ),
            ("stranger".into(), completed(Some(0), false, b"")),
            ("gone".into(), AgentOutcome::Offline),
            ("nonzero".into(), completed(Some(2), false, b"")),
            ("slow".into(), completed(None, true, b"partial")),
            ("ok".into(), completed(Some(0), false, b"hello \xff")),
            ("old".into(), AgentOutcome::Unsupported),
        ];
        let (results, summary) = aggregate(&requested, outcomes);

        let order: Vec<&str> = results.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(
            order,
            requested.iter().map(String::as_str).collect::<Vec<_>>()
        );
        let status: Vec<Status> = results.iter().map(|r| r.status).collect();
        assert_eq!(
            status,
            [
                Status::Completed,
                Status::Completed,
                Status::TimedOut,
                Status::Offline,
                Status::Unsupported,
                Status::Failed,
                Status::Failed,
            ]
        );
        assert_eq!(
            summary,
            Summary {
                total: 7,
                succeeded: 1,
                failed: 2,
                not_run: 4
            }
        );

        let ok = &results[0];
        assert_eq!(ok.exit_code, Some(0));
        assert_eq!(ok.stdout, "hello \u{fffd}", "invalid UTF-8 is replaced");
        assert_eq!(ok.stderr, "warn");
        assert!(ok.stderr_truncated && !ok.stdout_truncated);
        assert_eq!(ok.duration_ms, Some(12));
        assert_eq!(results[1].exit_code, Some(2));
        assert_eq!(results[2].exit_code, None);
        assert_eq!(results[2].stdout, "partial");
        assert_eq!(results[3].error.as_deref(), Some("agent is not connected"));
        assert_eq!(results[5].error.as_deref(), Some("no powershell"));
        assert_eq!(results[6].error.as_deref(), Some("no result"));
    }

    #[test]
    fn audit_detail_lists_each_agents_status_and_exit_code() {
        let requested = ids(&["a", "b"]);
        let (results, _) = aggregate(
            &requested,
            vec![
                ("a".into(), completed(Some(0), false, b"")),
                ("b".into(), AgentOutcome::Offline),
            ],
        );
        assert_eq!(
            audit_results(&results),
            json!({
                "a": {"status": "completed", "exit_code": 0},
                "b": {"status": "offline", "exit_code": null},
            })
        );
    }

    #[test]
    fn result_json_is_what_the_api_returns() {
        let (results, summary) = aggregate(
            &ids(&["a"]),
            vec![("a".into(), completed(Some(1), false, b"x"))],
        );
        assert_eq!(
            serde_json::to_value(&results[0]).unwrap(),
            json!({
                "agent_id": "a",
                "status": "completed",
                "exit_code": 1,
                "stdout": "x",
                "stderr": "warn",
                "stdout_truncated": false,
                "stderr_truncated": true,
                "duration_ms": 12,
                "error": null,
            })
        );
        assert_eq!(
            serde_json::to_value(summary).unwrap(),
            json!({"total": 1, "succeeded": 0, "failed": 1, "not_run": 0})
        );
    }

    #[tokio::test]
    async fn exchange_sends_the_script_and_reads_one_reply() {
        let (mut server_send, mut agent_recv) = tokio::io::duplex(1 << 16);
        let (mut agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        let request = ScriptRequest::new("hostname".into(), Some(5)).unwrap();
        let expected = request.clone();
        let agent = tokio::spawn(async move {
            let open: StreamOpen = read_frame(&mut agent_recv).await.unwrap().unwrap();
            assert_eq!(open, StreamOpen::Script(expected));
            // The server finished its side: nothing more comes.
            assert!(read_frame::<_, StreamOpen>(&mut agent_recv)
                .await
                .unwrap()
                .is_none());
            let reply = ScriptReply::Failed("nope".into());
            write_frame(&mut agent_send, &reply).await.unwrap();
        });
        assert_eq!(
            exchange(&mut server_send, &mut server_recv, &request).await,
            AgentOutcome::Reply(ScriptReply::Failed("nope".into()))
        );
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn exchange_reports_an_agent_that_hangs_up() {
        let (mut server_send, _agent_recv) = tokio::io::duplex(1 << 16);
        let (agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        drop(agent_send);
        let request = ScriptRequest::new("x".into(), Some(5)).unwrap();
        assert!(matches!(
            exchange(&mut server_send, &mut server_recv, &request).await,
            AgentOutcome::Error(e) if e.contains("without a result")
        ));
    }
}
