//! Remote shell, script runner and file transfer end to end: the real server
//! (Postgres, QUIC listener, HTTPS API), two real enrolled agents, and plain
//! HTTPS/WebSocket clients. No viewer is involved anywhere.
//!
//! The agents run the development fallbacks (`/bin/sh` on a Unix PTY,
//! `sh -c` for scripts), so this runs on Linux and macOS; the same streams
//! drive ConPTY and PowerShell on Windows.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::transfer::sha256;
use reqwest::StatusCode;
use serde_json::{json, Value};
use server::users::{CreatedUser, Role};
use support::{audit_actions, create_user, start_db, Api};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as Ws;

const DEADLINE: Duration = Duration::from_secs(30);

struct Rig {
    db: support::TestDb,
    certs: common::devcerts::DevCerts,
    api: Api,
    agents: Vec<String>,
    admin: CreatedUser,
    auditor: CreatedUser,
}

async fn start_agent(
    pool: &sqlx::PgPool,
    certs: &common::devcerts::DevCerts,
    addr: std::net::SocketAddr,
) -> String {
    let token = server::enroll::create_token(pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();
    let mut config = credential.agent_config(Duration::from_secs(5)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    let session = agent::connect(&config).await.unwrap();
    tokio::spawn(async move { session.run(None).await });
    credential.agent_id
}

async fn rig() -> Rig {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    let hub = quic.hub();
    std::mem::forget(quic);
    let api = support::start_api_full(
        db.pool.clone(),
        &certs,
        "/nonexistent-updates".into(),
        Some(hub.clone()),
    )
    .await;

    let mut agents = Vec::new();
    for _ in 0..2 {
        agents.push(start_agent(&db.pool, &certs, addr).await);
    }
    timeout(DEADLINE, async {
        while !agents.iter().all(|a| hub.is_online(a)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both agents connected");

    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let auditor = create_user(&db.pool, "reader", Role::Auditor).await;
    Rig {
        db,
        certs,
        api,
        agents,
        admin,
        auditor,
    }
}

impl Rig {
    async fn session(&self, user: &CreatedUser) -> String {
        self.api
            .session(&user.user.username, &user.totp_secret)
            .await
    }

    /// Audit details for `action`, oldest first.
    async fn audit(&self, action: &str) -> Vec<(String, Option<String>, Value)> {
        sqlx::query_as("SELECT actor, target, detail FROM audit_log WHERE action = $1 ORDER BY id")
            .bind(action)
            .fetch_all(&self.db.pool)
            .await
            .unwrap()
    }

    /// Poll until `action` has `n` rows (some are written after the
    /// response, when a stream is dropped).
    async fn wait_for_audit(&self, action: &str, n: usize) -> Vec<(String, Option<String>, Value)> {
        timeout(DEADLINE, async {
            loop {
                let rows = self.audit(action).await;
                if rows.len() >= n {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{n} {action} audit rows"))
    }

    fn tls(&self) -> rustls::ClientConfig {
        let mut roots = rustls::RootCertStore::empty();
        for ca in self.certs.ca().unwrap() {
            roots.add(ca).unwrap();
        }
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
    }

    /// Open the shell WebSocket as a plain client would.
    async fn shell(
        &self,
        token: &str,
        agent: &str,
        query: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
        tokio_tungstenite::tungstenite::Error,
    > {
        let url = self
            .api
            .url(&format!("/api/agents/{agent}/shell{query}"))
            .replace("https://", "wss://");
        let mut request = url.as_str().into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let port = url.split(':').nth(2).unwrap().split('/').next().unwrap();
        let tcp = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(self.tls()))
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .unwrap();
        tokio_tungstenite::client_async(request, tls)
            .await
            .map(|(ws, _)| ws)
    }

    async fn run_script(&self, token: &str, body: Value) -> reqwest::Response {
        self.api
            .client
            .post(self.api.url("/api/script-runs"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    fn files_url(&self, agent: &str, path: &std::path::Path, overwrite: bool) -> String {
        let mut url =
            reqwest::Url::parse(&self.api.url(&format!("/api/agents/{agent}/files"))).unwrap();
        url.query_pairs_mut()
            .append_pair("path", path.to_str().unwrap())
            .append_pair("overwrite", &overwrite.to_string());
        url.into()
    }
}

/// Deterministic, non-repeating-looking test data.
fn data(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x9E37_79B9;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[tokio::test]
async fn interactive_shell_over_a_websocket_with_resize() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    let agent = &rig.agents[0];
    let mut ws = rig
        .shell(&token, agent, "?cols=80&rows=24")
        .await
        .expect("shell opens");

    let first = timeout(DEADLINE, ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first, Ws::Text(r#"{"type":"started"}"#.into()));

    ws.send(Ws::Text(r#"{"type":"resize","cols":132,"rows":43}"#.into()))
        .await
        .unwrap();
    ws.send(Ws::Binary(
        b"stty size; echo marker-$((6*7)); exit 3\n".to_vec().into(),
    ))
    .await
    .unwrap();

    let mut output = Vec::new();
    let exit = timeout(DEADLINE, async {
        while let Some(msg) = ws.next().await {
            match msg.unwrap() {
                Ws::Binary(bytes) => output.extend_from_slice(&bytes),
                Ws::Text(text) => return serde_json::from_str::<Value>(&text).unwrap(),
                other => panic!("unexpected {other:?}"),
            }
        }
        panic!("closed without an exit message")
    })
    .await
    .expect("shell exits");
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("43 132"),
        "the resize reached the terminal: {text:?}"
    );
    assert!(text.contains("marker-42"), "{text:?}");
    assert_eq!(exit, json!({"type": "exit", "code": 3}));
    // The server closes the socket after the exit.
    assert!(matches!(
        timeout(DEADLINE, ws.next()).await.unwrap(),
        Some(Ok(Ws::Close(_))) | None
    ));

    let opens = rig.audit("shell.open").await;
    assert_eq!(opens.len(), 1);
    let (actor, target, detail) = &opens[0];
    assert_eq!(
        (actor.as_str(), target.as_deref()),
        ("root", Some(agent.as_str()))
    );
    assert_eq!(detail["started"], true);
    assert_eq!(
        (detail["cols"].clone(), detail["rows"].clone()),
        (json!(80), json!(24))
    );

    let closes = rig.wait_for_audit("shell.close", 1).await;
    let (_, target, detail) = &closes[0];
    assert_eq!(target.as_deref(), Some(agent.as_str()));
    assert_eq!(detail["exit_code"], 3);
    assert_eq!(detail["ended_by"], "exit");
    assert!(detail["bytes_in"].as_u64().unwrap() > 0);
    assert!(detail["bytes_out"].as_u64().unwrap() > 0);
    assert!(detail["shell_id"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn closing_the_websocket_kills_the_shell() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    let mut ws = rig.shell(&token, &rig.agents[0], "").await.unwrap();
    ws.next().await.unwrap().unwrap(); // started
    ws.close(None).await.unwrap();
    let closes = rig.wait_for_audit("shell.close", 1).await;
    assert_eq!(closes[0].2["ended_by"], "client");
    assert_eq!(closes[0].2["exit_code"], Value::Null);
}

#[tokio::test]
async fn a_script_fans_out_to_two_agents_and_returns_both_results() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    let (a, b) = (&rig.agents[0], &rig.agents[1]);
    // `$$` (the shell's pid) tells the two agents' runs apart.
    let script = "echo \"hello from $$\"; echo oops >&2; exit 5".to_owned();
    let resp = rig
        .run_script(
            &token,
            json!({ "agent_ids": [b, a, "agt-offline", a], "script": script, "timeout_secs": 30 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let report: Value = resp.json().await.unwrap();

    let results = report["results"].as_array().unwrap();
    let order: Vec<&str> = results
        .iter()
        .map(|r| r["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [b.as_str(), a.as_str(), "agt-offline"],
        "request order, deduplicated"
    );
    for r in &results[..2] {
        assert_eq!(r["status"], "completed", "{r}");
        assert_eq!(r["exit_code"], 5);
        assert!(
            r["stdout"].as_str().unwrap().starts_with("hello from "),
            "{r}"
        );
        assert_eq!(r["stderr"], "oops\n");
    }
    // Two separate processes, one per agent.
    assert_ne!(results[0]["stdout"], results[1]["stdout"]);
    assert_eq!(results[2]["status"], "offline");
    assert_eq!(
        report["summary"],
        json!({"total": 3, "succeeded": 0, "failed": 2, "not_run": 1})
    );

    // A single command on one agent, succeeding.
    let resp = rig
        .run_script(&token, json!({ "agent_ids": [a], "script": "uname -s" }))
        .await;
    let report: Value = resp.json().await.unwrap();
    assert_eq!(report["results"][0]["exit_code"], 0);
    assert_eq!(report["summary"]["succeeded"], 1);

    let runs = rig.audit("script.run").await;
    assert_eq!(runs.len(), 2);
    let (actor, _, detail) = &runs[0];
    assert_eq!(actor, "root");
    assert_eq!(detail["agents"], json!([b, a, "agt-offline"]));
    assert!(detail["summary"]
        .as_str()
        .unwrap()
        .starts_with("echo \"hello from $$\""));
    assert_eq!(detail["sha256"], hex::encode(sha256(script.as_bytes())));
    let completes = rig.audit("script.complete").await;
    let (_, _, detail) = &completes[0];
    assert_eq!(detail["run_id"], report_run_id(&rig, 0).await);
    assert_eq!(detail["results"][a]["exit_code"], 5);
    assert_eq!(detail["results"][b]["exit_code"], 5);
    assert_eq!(detail["results"]["agt-offline"]["status"], "offline");
}

/// The id of the n-th `script.run` row (a run's `run_id`).
async fn report_run_id(rig: &Rig, n: usize) -> Value {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM audit_log WHERE action = 'script.run' ORDER BY id")
            .fetch_all(&rig.db.pool)
            .await
            .unwrap();
    json!(ids[n])
}

#[tokio::test]
async fn scripts_time_out_and_bad_requests_are_rejected() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    let resp = rig
        .run_script(
            &token,
            json!({ "agent_ids": [&rig.agents[0]], "script": "echo started; sleep 60", "timeout_secs": 1 }),
        )
        .await;
    let report: Value = resp.json().await.unwrap();
    let r = &report["results"][0];
    assert_eq!(r["status"], "timed_out");
    assert_eq!(r["exit_code"], Value::Null);
    assert_eq!(r["stdout"], "started\n");

    for body in [
        json!({ "agent_ids": [], "script": "x" }),
        json!({ "agent_ids": ["a"], "script": "  " }),
        json!({ "agent_ids": ["a"], "script": "x", "timeout_secs": 0 }),
    ] {
        assert_eq!(
            rig.run_script(&token, body).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
}

#[tokio::test]
async fn a_large_file_transfers_both_ways_with_hash_verification() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    let agent = &rig.agents[1];
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("payload.bin");

    // 24 MiB and change: many chunks, streamed rather than buffered.
    let bytes = Arc::new(data(24 * 1024 * 1024 + 4321));
    let digest = hex::encode(sha256(&bytes));
    let body = {
        let bytes = bytes.clone();
        // Uneven pieces, so the server has to re-chunk.
        futures_util::stream::iter((0..bytes.len()).step_by(100_003).map(move |start| {
            let end = (start + 100_003).min(bytes.len());
            Ok::<_, std::io::Error>(bytes[start..end].to_vec())
        }))
    };
    let resp = rig
        .api
        .client
        .put(rig.files_url(agent, &target, false))
        .bearer_auth(&token)
        .header("content-length", bytes.len())
        .header("x-content-sha256", &digest)
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "{}",
        resp.text().await.unwrap()
    );
    let uploaded: Value = resp.json().await.unwrap();
    assert_eq!(
        uploaded,
        json!({ "path": target.to_str().unwrap(), "size": bytes.len(), "sha256": digest })
    );
    assert!(
        std::fs::read(&target).unwrap() == *bytes,
        "file on the agent matches"
    );

    // And back again.
    let resp = rig
        .api
        .client
        .get(rig.files_url(agent, &target, false))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-content-sha256"], digest.as_str());
    assert_eq!(resp.content_length(), Some(bytes.len() as u64));
    assert_eq!(
        resp.headers()["content-disposition"],
        "attachment; filename=\"payload.bin\""
    );
    let mut stream = resp.bytes_stream();
    let mut downloaded = Vec::with_capacity(bytes.len());
    while let Some(piece) = stream.next().await {
        downloaded.extend_from_slice(&piece.unwrap());
    }
    assert_eq!(hex::encode(sha256(&downloaded)), digest);
    assert!(downloaded == *bytes);

    let ups = rig.audit("file.upload").await;
    let (actor, target_agent, detail) = &ups[0];
    assert_eq!(
        (actor.as_str(), target_agent.as_deref()),
        ("root", Some(agent.as_str()))
    );
    assert_eq!(detail["path"], target.to_str().unwrap());
    assert_eq!(detail["bytes"], bytes.len());
    assert_eq!(detail["status"], "completed");
    assert_eq!(detail["sha256"], digest);
    let downs = rig.wait_for_audit("file.download", 1).await;
    let detail = &downs[0].2;
    assert_eq!(detail["bytes"], bytes.len());
    assert_eq!(detail["status"], "completed");
}

#[tokio::test]
async fn transfers_that_do_not_verify_are_refused_and_leave_no_file() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    let agent = &rig.agents[0];
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("f.bin");
    let bytes = data(700_000);
    let put = |sha: String, body: Vec<u8>, overwrite: bool| {
        rig.api
            .client
            .put(rig.files_url(agent, &target, overwrite))
            .bearer_auth(&token)
            .header("x-content-sha256", sha)
            .body(body)
            .send()
    };

    // Wrong hash: the agent verifies before the file appears.
    let resp = put(hex::encode(sha256(b"something else")), bytes.clone(), false)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

    // Missing hash header.
    let resp = rig
        .api
        .client
        .put(rig.files_url(agent, &target, false))
        .bearer_auth(&token)
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Existing file without overwrite, then with.
    std::fs::write(&target, b"old").unwrap();
    let good = hex::encode(sha256(&bytes));
    assert_eq!(
        put(good.clone(), bytes.clone(), false)
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(
        put(good, bytes.clone(), true).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(std::fs::read(&target).unwrap(), bytes);

    // Downloading something that is not there.
    let resp = rig
        .api
        .client
        .get(rig.files_url(agent, &dir.path().join("missing"), false))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let statuses: Vec<String> = rig
        .audit("file.upload")
        .await
        .iter()
        .map(|(_, _, d)| d["status"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(statuses, ["failed", "failed", "completed"]);
    let downs = rig.audit("file.download").await;
    assert_eq!(downs[0].2["status"], "failed");
}

#[tokio::test]
async fn unauthorized_roles_are_rejected_and_audited() {
    let rig = rig().await;
    let token = rig.session(&rig.auditor).await;
    let agent = &rig.agents[0];
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nope.txt");

    // Shell: refused before any upgrade.
    match rig.shell(&token, agent, "").await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::FORBIDDEN)
        }
        other => panic!("expected 403, got {:?}", other.map(|_| ())),
    }
    let resp = rig
        .run_script(
            &token,
            json!({ "agent_ids": [agent], "script": "touch /tmp/pwned" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = rig
        .api
        .client
        .put(rig.files_url(agent, &target, false))
        .bearer_auth(&token)
        .header("x-content-sha256", hex::encode(sha256(b"x")))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = rig
        .api
        .client
        .get(rig.files_url(agent, &target, false))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(!target.exists());

    let denied = rig.audit("permission.denied").await;
    let capabilities: Vec<&str> = denied
        .iter()
        .map(|(_, _, d)| d["capability"].as_str().unwrap())
        .collect();
    assert_eq!(
        capabilities,
        ["shell", "script", "file_transfer", "file_transfer"]
    );
    for (actor, target, detail) in &denied {
        assert_eq!(actor, "reader");
        assert_eq!(target.as_deref(), Some(agent.as_str()));
        assert_eq!(detail["role"], "auditor");
    }
    // Nothing was run or moved.
    let actions = audit_actions(&rig.db.pool).await;
    for action in ["shell.open", "script.run", "file.upload", "file.download"] {
        assert!(
            !actions.iter().any(|a| a == action),
            "{action} in {actions:?}"
        );
    }

    // No session at all is a 401, not an audited refusal.
    let resp = rig
        .api
        .client
        .post(rig.api.url("/api/script-runs"))
        .json(&json!({ "agent_ids": [agent], "script": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn offline_agents_are_a_conflict_not_a_hang() {
    let rig = rig().await;
    let token = rig.session(&rig.admin).await;
    match rig.shell(&token, "agt-nowhere", "").await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::CONFLICT)
        }
        other => panic!("expected 409, got {:?}", other.map(|_| ())),
    }
    let resp = rig
        .api
        .client
        .get(rig.files_url("agt-nowhere", std::path::Path::new("/etc/hostname"), false))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "agent is not connected");
}

#[tokio::test]
async fn agent_list_shows_hostname_online_state_and_open_shells() {
    let rig = rig().await;
    let token = rig.session(&rig.auditor).await;
    let list = || async {
        rig.api
            .client
            .get(rig.api.url("/api/agents"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json::<Vec<Value>>()
            .await
            .unwrap()
    };
    let find = |agents: &[Value], id: &str| -> Value {
        agents.iter().find(|a| a["id"] == id).unwrap().clone()
    };
    let expected_host = agent::device::hostname().unwrap();
    let agents = timeout(DEADLINE, async {
        loop {
            let agents = list().await;
            // AgentInfo is processed shortly after the agent comes online.
            if agents.iter().all(|a| a["hostname"].is_string()) {
                return agents;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("hostnames recorded");
    for id in &rig.agents {
        let agent = find(&agents, id);
        assert_eq!(agent["hostname"], expected_host.as_str());
        assert_eq!(agent["online"], true);
        assert_eq!(agent["viewer_sessions"], 0);
        assert_eq!(agent["shell_sessions"], 0);
    }

    // An open shell counts against its agent only, until it closes.
    let admin = rig.session(&rig.admin).await;
    let mut ws = rig.shell(&admin, &rig.agents[0], "").await.unwrap();
    let agents = list().await;
    assert_eq!(find(&agents, &rig.agents[0])["shell_sessions"], 1);
    assert_eq!(find(&agents, &rig.agents[1])["shell_sessions"], 0);
    ws.close(None).await.unwrap();
    timeout(DEADLINE, async {
        while find(&list().await, &rig.agents[0])["shell_sessions"] != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("shell count drops after close");
}
