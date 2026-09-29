//! Prometheus metrics end to end: real agents (one per transport), real API
//! traffic, then a scrape of `/metrics` over HTTP.
//!
//! Metrics are process-wide, so this binary holds a single test.

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use server::users::Role;
use support::{create_user, start_api_full, start_db};

async fn start_agent(
    pool: &sqlx::PgPool,
    certs: &common::devcerts::DevCerts,
    addr: std::net::SocketAddr,
    transport: transport::TransportSettings,
) {
    let token = server::enroll::create_token(pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        transport,
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();
    let mut config = credential.agent_config(Duration::from_millis(100)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    let session = agent::connect(&config).await.unwrap();
    tokio::spawn(async move { session.run(None).await });
}

/// The value of the sample line starting with `series`.
fn sample(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' ')?.parse().ok())
}

#[tokio::test]
async fn metrics_track_connections_logins_denials_and_requests() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    let ws_addr = quic.ws_local_addr();
    let api = start_api_full(
        db.pool.clone(),
        &certs,
        "/nonexistent".into(),
        Some(quic.hub()),
    )
    .await;
    std::mem::forget(quic);
    for mode in [
        transport::TransportMode::Quic,
        transport::TransportMode::WebSocket,
    ] {
        let settings = transport::TransportSettings { mode, ws_addr };
        start_agent(&db.pool, &certs, addr, settings).await;
    }

    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let auditor = create_user(&db.pool, "reader", Role::Auditor).await;
    let admin = api.session("root", &admin.totp_secret).await;
    let auditor = api.session("reader", &auditor.totp_secret).await;
    // One wrong password.
    let resp = api
        .client
        .post(api.url("/api/auth/login"))
        .json(&json!({ "username": "root", "password": "not the password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let agents: Vec<Value> = api
        .client
        .get(api.url("/api/agents"))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // An auditor may not run scripts: denied and counted.
    let resp = api
        .client
        .post(api.url("/api/script-runs"))
        .bearer_auth(&auditor)
        .json(&json!({ "agent_ids": [agents[0]["id"]], "script": "hostname" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_addr = listener.local_addr().unwrap();
    tokio::spawn(server::metrics::serve(listener, db.pool.clone()));
    let scrape = || async {
        let resp = api
            .client
            .get(format!("http://{metrics_addr}/metrics"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/openmetrics-text"));
        resp.text().await.unwrap()
    };

    // Heartbeats arrive every 100 ms; wait for a few.
    let text = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let text = scrape().await;
            if sample(&text, "rmm_agent_heartbeats_total").unwrap_or(0.0) >= 4.0 {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("heartbeats counted");

    let expect = |series: &str, value: f64| {
        assert_eq!(sample(&text, series), Some(value), "{series} in:\n{text}");
    };
    expect("rmm_agents_connected{transport=\"quic\"}", 1.0);
    expect("rmm_agents_connected{transport=\"websocket\"}", 1.0);
    expect("rmm_agents_registered", 2.0);
    expect("rmm_enrollments_total{result=\"enrolled\"}", 2.0);
    expect("rmm_logins_total{result=\"success\"}", 2.0);
    expect("rmm_logins_total{result=\"bad_password\"}", 1.0);
    expect("rmm_permission_denied_total{capability=\"script\"}", 1.0);
    expect(
        "rmm_http_requests_total{method=\"GET\",route=\"/api/agents\",status=\"200\"}",
        1.0,
    );
    expect(
        "rmm_http_requests_total{method=\"POST\",route=\"/api/script-runs\",status=\"403\"}",
        1.0,
    );
    assert!(sample(&text, "rmm_audit_entries_total").unwrap() >= 6.0);
    assert!(
        text.contains("rmm_http_request_duration_seconds_bucket{le="),
        "{text}"
    );
    // No agent ids, user names or paths with ids in them.
    let id = agents[0]["id"].as_str().unwrap();
    assert!(!text.contains(id) && !text.contains("root"), "{text}");
}
