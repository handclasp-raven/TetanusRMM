//! Enrollment end to end: download links, one-time tokens over QUIC, issued
//! certificates, credential storage, and certificate pinning on reconnect.

mod support;

use std::time::Duration;

use agent::credstore::{Credential, CredentialStore};
use agent::enroll::EnrollOptions;
use agent::{AgentError, AgentEvent};
use common::devcerts::DevCerts;
use reqwest::StatusCode;
use serde_json::{json, Value};
use server::audit::{self, Verification};
use server::enroll;
use server::registry::{self, ConsentMode, EnrollmentState};
use server::users::Role;
use server::ServerEvent;
use sqlx::PgPool;
use support::{audit_actions, create_user, start_api_with, start_db, start_quic};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);

fn enroll_opts(addr: std::net::SocketAddr, certs: &DevCerts, token: &str) -> EnrollOptions {
    EnrollOptions {
        server_addr: addr,
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token: token.to_owned(),
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    }
}

async fn new_token(pool: &PgPool) -> String {
    enroll::create_token(pool, "alice", Duration::from_secs(3600))
        .await
        .unwrap()
        .token
}

/// Connect with `config`, run the heartbeat loop in the background, and
/// return the ack stream.
async fn run_agent(
    mut config: agent::AgentConfig,
) -> Result<UnboundedReceiver<AgentEvent>, AgentError> {
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.heartbeat_interval = Duration::from_millis(50);
    let session = agent::connect(&config).await?;
    let (tx, rx) = unbounded_channel();
    tokio::spawn(async move { session.run(Some(tx)).await });
    Ok(rx)
}

/// Connect with `config` and expect the server to refuse the Hello with `code`.
async fn assert_rejected(mut config: agent::AgentConfig, code: u32) {
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    let session = agent::connect(&config).await.unwrap();
    let result = timeout(DEADLINE, session.run(None))
        .await
        .expect("rejection should not hang");
    assert!(result.is_err(), "Hello should have been refused");
    match session.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(close)) => {
            assert_eq!(u64::from(close.error_code), u64::from(code), "{close:?}");
        }
        other => panic!("expected an application close, got {other:?}"),
    }
}

async fn wait_for_hello(events: &mut UnboundedReceiver<ServerEvent>) -> String {
    timeout(DEADLINE, async {
        loop {
            if let Some(ServerEvent::Hello { agent_id, .. }) = events.recv().await {
                return agent_id;
            }
        }
    })
    .await
    .expect("Hello")
}

async fn wait_for_acks(acks: &mut UnboundedReceiver<AgentEvent>, n: usize) {
    timeout(DEADLINE, async {
        for _ in 0..n {
            acks.recv().await.expect("ack");
        }
    })
    .await
    .expect("acks");
}

#[tokio::test]
async fn enrollment_token_is_single_use() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, mut events) = start_quic(db.pool.clone(), &certs);
    let token = new_token(&db.pool).await;

    let credential = agent::enroll::enroll(&enroll_opts(addr, &certs, &token))
        .await
        .unwrap();
    assert!(credential.agent_id.starts_with("agt-"));
    assert!(matches!(
        timeout(DEADLINE, events.recv()).await.unwrap(),
        Some(ServerEvent::Enrolled { agent_id, .. }) if agent_id == credential.agent_id
    ));

    // Second use of the same token is refused, with the server's reason.
    let second = agent::enroll::enroll(&enroll_opts(addr, &certs, &token)).await;
    match second {
        Err(AgentError::Enroll(reason)) => assert_eq!(reason, "invalid enrollment token"),
        other => panic!("expected rejection, got {other:?}"),
    }

    let agents = registry::list_agents(&db.pool).await.unwrap();
    assert_eq!(agents.len(), 1);
    let (used, token_agent): (bool, Option<String>) =
        sqlx::query_as("SELECT used_at IS NOT NULL, agent_id FROM enrollment_tokens")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(used);
    assert_eq!(token_agent.as_deref(), Some(credential.agent_id.as_str()));
    assert_eq!(
        audit_actions(&db.pool).await,
        ["enrollment.create", "agent.enroll"]
    );
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { entries: 2 }
    );
}

#[tokio::test]
async fn concurrent_enrollments_with_one_token_yield_one_agent() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, _events) = start_quic(db.pool.clone(), &certs);
    let token = new_token(&db.pool).await;

    let attempts: Vec<_> = (0..5)
        .map(|_| {
            let opts = enroll_opts(addr, &certs, &token);
            tokio::spawn(async move { agent::enroll::enroll(&opts).await })
        })
        .collect();
    let mut successes = 0;
    for attempt in attempts {
        if attempt.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(registry::list_agents(&db.pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn expired_and_unknown_tokens_are_rejected() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, _events) = start_quic(db.pool.clone(), &certs);
    let token = new_token(&db.pool).await;
    sqlx::query("UPDATE enrollment_tokens SET expires_at = now() - interval '1 second'")
        .execute(&db.pool)
        .await
        .unwrap();

    for bad in [token.as_str(), "not-a-real-token"] {
        let result = agent::enroll::enroll(&enroll_opts(addr, &certs, bad)).await;
        assert!(matches!(result, Err(AgentError::Enroll(_))), "{result:?}");
    }
    assert!(registry::list_agents(&db.pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn enrolled_agent_reconnects_using_its_issued_cert() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, mut events) = start_quic(db.pool.clone(), &certs);
    let token = new_token(&db.pool).await;

    // Enroll, persist the credential, and load it back as a restart would.
    let enrolled = agent::enroll::enroll(&enroll_opts(addr, &certs, &token))
        .await
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    let store = CredentialStore::new(state.path());
    store.save(&enrolled).unwrap();
    let credential: Credential = store.load().unwrap();
    assert_eq!(credential, enrolled);
    assert_eq!(credential.api_url, "https://localhost:8443");

    let mut acks = run_agent(credential.agent_config(Duration::from_secs(5)).unwrap())
        .await
        .unwrap();
    // Skip the Enrolled event from the first connection.
    assert_eq!(wait_for_hello(&mut events).await, credential.agent_id);
    wait_for_acks(&mut acks, 2).await;

    let agent = registry::list_agents(&db.pool).await.unwrap().remove(0);
    assert_eq!(agent.id, credential.agent_id);
    assert_eq!(agent.enrollment_state, EnrollmentState::Enrolled);
    assert!(agent.last_seen.is_some());
    let issued = common::tls::certs_from_pem(&credential.cert_pem).unwrap();
    assert_eq!(
        agent.cert_fingerprint.as_deref(),
        Some(server::quic::cert_fingerprint(&issued[0]).as_str())
    );
    assert_eq!(
        registry::get_policy(&db.pool, &agent.id)
            .await
            .unwrap()
            .unwrap()
            .consent_mode,
        ConsentMode::Notify
    );
}

#[tokio::test]
async fn hello_requires_the_pinned_certificate() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, mut events) = start_quic(db.pool.clone(), &certs);
    let token = new_token(&db.pool).await;
    let credential = agent::enroll::enroll(&enroll_opts(addr, &certs, &token))
        .await
        .unwrap();
    let config = || credential.agent_config(Duration::from_secs(5)).unwrap();
    let unauthorized = protocol::close_code::UNAUTHORIZED;

    // 1. A certificate from our CA that was never issued through enrollment.
    let mut stray = config();
    stray.identity = certs.agent_identity().unwrap();
    assert_rejected(stray, unauthorized).await;

    // 2. The issued certificate, claiming a different agent id.
    let mut impostor = config();
    impostor.agent_id = "agt-someone-else".into();
    assert_rejected(impostor, unauthorized).await;

    // 3. The right certificate after the agent has been revoked.
    sqlx::query("UPDATE agents SET enrollment_state = 'revoked'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_rejected(config(), unauthorized).await;

    // None of these produced a Hello.
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, ServerEvent::Hello { .. }), "{event:?}");
    }

    // Reinstating the agent lets the same certificate back in.
    sqlx::query("UPDATE agents SET enrollment_state = 'enrolled'")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut acks = run_agent(config()).await.unwrap();
    wait_for_acks(&mut acks, 1).await;
}

#[tokio::test]
async fn hello_without_a_certificate_is_rejected() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, _events) = start_quic(db.pool.clone(), &certs);

    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(
        common::quic::client_config(&certs.ca().unwrap(), None).unwrap(),
    );
    let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    protocol::write_frame(
        &mut send,
        &protocol::Message::Hello {
            agent_id: "agt-anon".into(),
            version: protocol::PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    match timeout(DEADLINE, conn.closed()).await.unwrap() {
        quinn::ConnectionError::ApplicationClosed(close) => {
            assert_eq!(
                u64::from(close.error_code),
                u64::from(protocol::close_code::UNAUTHORIZED)
            );
        }
        other => panic!("expected application close, got {other:?}"),
    }
}

#[tokio::test]
async fn download_links_are_role_checked_and_expire_with_their_token() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let engineer = create_user(&db.pool, "sam", Role::SupportEngineer).await;
    let auditor = create_user(&db.pool, "carol", Role::Auditor).await;

    // A published agent build for the link to download.
    let updates = tempfile::tempdir().unwrap();
    let build = updates.path().join("build.exe");
    std::fs::write(&build, b"agent build bytes").unwrap();
    let key = server::updates::generate_key();
    server::updates::write_key_pair(updates.path(), &key).unwrap();
    server::updates::sign_file(
        &updates.path().join(server::updates::SIGNING_KEY_FILE),
        &build,
        "windows-x86_64",
        "0.1.0",
    )
    .unwrap();
    server::updates::publish(updates.path(), &build, "windows-x86_64", "0.1.0").unwrap();

    let api = start_api_with(db.pool.clone(), &certs, updates.path().to_owned()).await;
    let engineer_token = api.session("sam", &engineer.totp_secret).await;
    let auditor_token = api.session("carol", &auditor.totp_secret).await;
    let create = |token: &str, body: Value| {
        api.client
            .post(api.url("/api/enrollment-links"))
            .bearer_auth(token)
            .json(&body)
            .send()
    };

    assert_eq!(
        create(&auditor_token, json!({})).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        create(&engineer_token, json!({ "ttl_secs": 0 }))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let resp = create(&engineer_token, json!({ "ttl_secs": 600 }))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let link: Value = resp.json().await.unwrap();
    let token = link["token"].as_str().unwrap();
    let download_url = link["download_url"].as_str().unwrap();
    assert_eq!(
        download_url,
        format!("{}/api/download/windows-x86_64?token={token}", api.base)
    );

    // The link downloads the published build.
    let resp = api.client.get(download_url).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["content-disposition"],
        "attachment; filename=\"rmm-agent.exe\""
    );
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"agent build bytes");

    // A made-up token downloads nothing.
    let resp = api
        .client
        .get(api.url("/api/download/windows-x86_64?token=nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Once the token has been used to enroll, the link is dead.
    let ca = enroll::AgentCa::from_pem(&certs.ca_cert, &certs.ca_key).unwrap();
    let csr = rcgen::CertificateParams::default()
        .serialize_request(&rcgen::KeyPair::generate().unwrap())
        .unwrap();
    enroll::enroll(&db.pool, &ca, token, csr.der())
        .await
        .unwrap();
    let resp = api.client.get(download_url).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let actions = audit_actions(&db.pool).await;
    assert_eq!(
        actions.iter().filter(|a| *a == "enrollment.create").count(),
        1
    );
    assert_eq!(actions.last().unwrap(), "agent.enroll");
}
