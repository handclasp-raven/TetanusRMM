//! Quick assist end to end: a technician's code, redeemed over QUIC by a
//! client with no certificate, becomes a throwaway agent that only that
//! technician (and admins) can reach, for remote desktop and file transfer
//! only, and that is deleted when it has gone.

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use agent::credstore::Credential;
use agent::enroll::EnrollOptions;
use agent::AgentError;
use common::devcerts::DevCerts;
use protocol::assist::{read_trailer, REJECT_CODE, REJECT_TOO_MANY};
use reqwest::StatusCode;
use serde_json::{json, Value};
use server::assist::{self, MAX_FAILURES_PER_IP};
use server::audit::{self, Verification};
use server::users::{CreatedUser, Role};
use support::{audit_actions, create_user, start_db, Api, TestDb};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);

struct Rig {
    db: TestDb,
    certs: DevCerts,
    api: Api,
    addr: SocketAddr,
    hub: std::sync::Arc<server::relay::Hub>,
    updates: tempfile::TempDir,
}

async fn rig() -> Rig {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    let hub = quic.hub();
    std::mem::forget(quic);
    let updates = tempfile::tempdir().unwrap();
    let api = support::start_api_full(
        db.pool.clone(),
        &certs,
        updates.path().to_owned(),
        Some(hub.clone()),
    )
    .await;
    Rig {
        db,
        certs,
        api,
        addr,
        hub,
        updates,
    }
}

impl Rig {
    async fn session(&self, user: &CreatedUser) -> String {
        self.api
            .session(&user.user.username, &user.totp_secret)
            .await
    }

    /// A new code, as the API hands it out: `(session id, code)`.
    async fn code(&self, token: &str) -> (i64, String) {
        let resp = self
            .api
            .client
            .post(self.api.url("/api/assist-sessions"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["url"], format!("{}/assist", self.api.base));
        (
            body["id"].as_i64().unwrap(),
            body["code"].as_str().unwrap().to_owned(),
        )
    }

    async fn status(&self, token: &str, id: i64) -> reqwest::Response {
        self.api
            .client
            .get(self.api.url(&format!("/api/assist-sessions/{id}")))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn state(&self, token: &str, id: i64) -> Value {
        let resp = self.status(token, id).await;
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json().await.unwrap()
    }

    fn opts(&self, code: &str) -> EnrollOptions {
        EnrollOptions {
            server_addr: self.addr,
            transport: Default::default(),
            server_name: "localhost".into(),
            server_ca_pem: self.certs.ca_cert.clone(),
            token: code.to_owned(),
            bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        }
    }

    async fn redeem(&self, code: &str) -> Result<Credential, AgentError> {
        agent::enroll::enroll_assist(&self.opts(code)).await
    }

    /// Run the quick assist client's connection until the task is aborted.
    async fn connect(&self, credential: &Credential) -> tokio::task::JoinHandle<()> {
        let mut config = credential.agent_config(Duration::from_millis(50)).unwrap();
        config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
        config.remote = agent::remote::Allowed::FilesOnly;
        let session = agent::connect(&config).await.unwrap();
        let task = tokio::spawn(async move {
            let _ = session.run(None).await;
            session.close();
        });
        let hub = self.hub.clone();
        let agent_id = credential.agent_id.clone();
        timeout(DEADLINE, async move {
            while !hub.is_online(&agent_id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("quick assist agent connected");
        task
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> StatusCode {
        self.api
            .client
            .post(self.api.url(path))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }
}

fn rejection(result: Result<Credential, AgentError>) -> String {
    match result {
        Err(AgentError::Enroll(reason)) => reason,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// A six-digit code that is not `code`.
fn another(code: &str) -> String {
    format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000)
}

#[tokio::test]
async fn a_code_becomes_a_session_only_its_technician_can_use() {
    let rig = rig().await;
    let engineer = create_user(&rig.db.pool, "eve", Role::SupportEngineer).await;
    let other = create_user(&rig.db.pool, "oscar", Role::SupportEngineer).await;
    let admin = create_user(&rig.db.pool, "root", Role::Admin).await;
    let auditor = create_user(&rig.db.pool, "reader", Role::Auditor).await;
    let token = rig.session(&engineer).await;
    let other_token = rig.session(&other).await;
    let admin_token = rig.session(&admin).await;
    let auditor_token = rig.session(&auditor).await;

    // Auditors cannot make codes, and nobody can without signing in.
    assert_eq!(
        rig.post(&auditor_token, "/api/assist-sessions", json!({}))
            .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rig.post("", "/api/assist-sessions", json!({})).await,
        StatusCode::UNAUTHORIZED
    );

    let (id, code) = rig.code(&token).await;
    assert_eq!(protocol::assist::normalize_code(&code), Some(code.clone()));
    assert_eq!(rig.state(&token, id).await["status"], "waiting");
    // Someone else's session does not exist, except to an admin.
    assert_eq!(
        rig.status(&other_token, id).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(rig.state(&admin_token, id).await["status"], "waiting");

    // The user types it with a space in the middle.
    let typed = format!("{} {}", &code[..3], &code[3..]);
    let credential = rig.redeem(&typed).await.unwrap();
    assert!(credential.agent_id.starts_with("qa-"));
    let state = rig.state(&token, id).await;
    assert_eq!(state["status"], "connecting");
    assert_eq!(state["agent_id"], credential.agent_id.as_str());
    // The code is spent.
    assert_eq!(rejection(rig.redeem(&code).await), REJECT_CODE);

    let agent = rig.connect(&credential).await;
    assert_eq!(rig.state(&token, id).await["status"], "connected");

    // The user is always asked first.
    let policy = server::registry::get_policy(&rig.db.pool, &credential.agent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(policy.consent_mode, server::registry::ConsentMode::Require);
    assert_eq!(policy.on_no_user, server::registry::OnNoUser::Deny);

    // Its technician sees it, with desktop and file transfer and nothing
    // else, and needs no grant for a viewer.
    let listed = |token: String| {
        let rig = &rig;
        async move {
            let agents: Vec<Value> = rig
                .api
                .client
                .get(rig.api.url("/api/agents"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            agents
        }
    };
    let mine = listed(token.clone()).await;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["id"], credential.agent_id.as_str());
    assert_eq!(mine[0]["capabilities"], json!(["desktop", "file_transfer"]));
    assert!(mine[0]["assist_session_id"].is_i64());
    let viewer = format!("/api/agents/{}/viewer-sessions", credential.agent_id);
    assert_eq!(rig.post(&token, &viewer, json!({})).await, StatusCode::OK);

    // Another engineer does not, even one granted everything everywhere.
    server::access::create_grant(
        &rig.db.pool,
        "root",
        other.user.id,
        server::access::Scope::AllAgents,
        &server::access::all_capabilities(),
    )
    .await
    .unwrap();
    assert!(listed(other_token.clone()).await.is_empty());
    assert_eq!(
        rig.post(&other_token, &viewer, json!({})).await,
        StatusCode::FORBIDDEN
    );

    // An admin may view it too, but nobody may run things on it.
    let admins = listed(admin_token.clone()).await;
    assert_eq!(
        admins[0]["capabilities"],
        json!(["desktop", "file_transfer"])
    );
    assert_eq!(
        rig.post(&admin_token, &viewer, json!({})).await,
        StatusCode::OK
    );
    for token in [&admin_token, &token] {
        assert_eq!(
            rig.post(
                token,
                "/api/script-runs",
                json!({ "agent_ids": [credential.agent_id], "script": "whoami" }),
            )
            .await,
            StatusCode::FORBIDDEN
        );
        let shell = rig
            .api
            .client
            .get(
                rig.api
                    .url(&format!("/api/agents/{}/shell", credential.agent_id)),
            )
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(shell.status(), StatusCode::FORBIDDEN);
    }

    // File transfer is allowed through to the agent (which has no such file).
    let download = rig
        .api
        .client
        .get(rig.api.url(&format!(
            "/api/agents/{}/files?path=/nonexistent-quick-assist-file",
            credential.agent_id
        )))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::NOT_FOUND);

    // Still connected: nothing to prune. Once it has gone, it is deleted,
    // and with it the certificate's standing.
    assert!(assist::prune(&rig.db.pool, Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
    agent.abort();
    let _ = agent.await;
    timeout(DEADLINE, async {
        while rig.hub.is_online(&credential.agent_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("agent disconnected");
    assert_eq!(rig.state(&token, id).await["status"], "connecting");
    // Offline quick assist agents are not listed.
    assert!(listed(token.clone()).await.is_empty());
    assert_eq!(
        assist::prune(&rig.db.pool, Duration::ZERO).await.unwrap(),
        std::slice::from_ref(&credential.agent_id)
    );
    assert_eq!(rig.state(&token, id).await["status"], "ended");
    assert!(
        server::registry::get_agent(&rig.db.pool, &credential.agent_id)
            .await
            .unwrap()
            .is_none()
    );

    let actions = audit_actions(&rig.db.pool).await;
    for action in ["assist.create", "assist.redeem", "assist.end"] {
        assert!(
            actions.iter().any(|a| a == action),
            "{action} in {actions:?}"
        );
    }
    assert!(matches!(
        audit::verify(&rig.db.pool).await.unwrap(),
        Verification::Valid { .. }
    ));
}

#[tokio::test]
async fn wrong_and_expired_codes_are_refused_and_guessing_is_limited() {
    let rig = rig().await;
    let engineer = create_user(&rig.db.pool, "eve", Role::SupportEngineer).await;
    let token = rig.session(&engineer).await;

    // An expired code is as good as none.
    let (expired_id, expired) = rig.code(&token).await;
    sqlx::query("UPDATE assist_sessions SET expires_at = now() - interval '1 second'")
        .execute(&rig.db.pool)
        .await
        .unwrap();
    assert_eq!(rig.state(&token, expired_id).await["status"], "expired");
    assert_eq!(rejection(rig.redeem(&expired).await), REJECT_CODE);

    let (id, code) = rig.code(&token).await;
    for _ in 1..MAX_FAILURES_PER_IP - 1 {
        assert_eq!(rejection(rig.redeem(&another(&code)).await), REJECT_CODE);
    }
    assert_eq!(rejection(rig.redeem("not a code").await), REJECT_CODE);
    // That was the last wrong code this address gets: now even the right
    // one is refused.
    assert_eq!(rejection(rig.redeem(&code).await), REJECT_TOO_MANY);
    assert_eq!(rig.state(&token, id).await["status"], "waiting");
    assert!(server::registry::list_agents(&rig.db.pool)
        .await
        .unwrap()
        .is_empty());

    // Too many wrong codes from everywhere void what is outstanding.
    assert_eq!(assist::void_all(&rig.db.pool, 100).await.unwrap(), 1);
    assert_eq!(rig.state(&token, id).await["status"], "expired");
    assert!(audit_actions(&rig.db.pool)
        .await
        .contains(&"assist.lockout".to_owned()));
}

#[tokio::test]
async fn the_download_carries_this_servers_settings() {
    let rig = rig().await;
    let get = |path: &'static str| {
        let rig = &rig;
        async move { rig.api.client.get(rig.api.url(path)).send().await.unwrap() }
    };

    // Nothing published yet.
    let page = get("/assist").await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page
        .text()
        .await
        .unwrap()
        .contains("has not been published"));
    assert_eq!(
        get("/assist/download").await.status(),
        StatusCode::NOT_FOUND
    );

    let build = rig.updates.path().join("build.exe");
    std::fs::write(&build, b"MZ quick assist build").unwrap();
    server::updates::publish_assist(rig.updates.path(), &build, "windows-x86_64", "0.4.0").unwrap();

    // No sign-in needed: the user has no account.
    let page = get("/assist").await.text().await.unwrap();
    assert!(page.contains("href=\"/assist/download\"") && page.contains("0.4.0"));
    assert!(page.contains("gift cards"));

    let resp = get("/assist/download").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("TetanusRMM-Assist.exe"));
    let length: usize = resp.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let file = resp.bytes().await.unwrap();
    assert_eq!(file.len(), length);
    assert!(file.starts_with(b"MZ quick assist build"));
    let config = read_trailer(&file).expect("a config at the end");
    assert_eq!(config.server, "localhost:4433");
    assert_eq!(config.server_name, "localhost");
    assert_eq!(config.ca_pem, rig.certs.ca_cert);
}
