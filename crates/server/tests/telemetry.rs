//! Agent telemetry end to end: heartbeat over QUIC, stored on the agents row,
//! read back through the HTTPS API.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agent::telemetry::TelemetrySource;
use protocol::Telemetry;
use serde_json::Value;
use server::enroll;
use server::users::Role;
use support::{create_user, start_api_with, start_db, start_quic};
use tokio::time::timeout;

/// Deterministic telemetry whose uptime counts up with each sample.
struct Fixed {
    samples: AtomicU64,
}

impl TelemetrySource for Fixed {
    fn sample(&self) -> Telemetry {
        let n = self.samples.fetch_add(1, Ordering::SeqCst);
        Telemetry {
            cpu_percent: 42.5,
            mem_used_bytes: 6 << 30,
            mem_total_bytes: 16 << 30,
            disk_used_bytes: 120 << 30,
            disk_total_bytes: 500 << 30,
            uptime_secs: 1000 + n,
        }
    }
}

#[tokio::test]
async fn heartbeat_telemetry_is_stored_and_served_by_the_api() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (_quic, addr, _events) = start_quic(db.pool.clone(), &certs);
    let token = enroll::create_token(&db.pool, "alice", Duration::from_secs(600))
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

    // Before any heartbeat, telemetry columns are empty.
    let admin = create_user(&db.pool, "root", Role::Auditor).await;
    let api = start_api_with(db.pool.clone(), &certs, "/nonexistent".into()).await;
    let session = api.session("root", &admin.totp_secret).await;
    let agents = |api: &support::Api, session: String| {
        let req = api.client.get(api.url("/api/agents")).bearer_auth(session);
        async move {
            req.send()
                .await
                .unwrap()
                .json::<Vec<Value>>()
                .await
                .unwrap()
        }
    };
    assert!(agents(&api, session.clone()).await[0]["cpu_percent"].is_null());

    let mut config = credential.agent_config(Duration::from_millis(50)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.telemetry = Some(Arc::new(Fixed {
        samples: AtomicU64::new(0),
    }));
    let agent = agent::connect(&config).await.unwrap();
    let (acks_tx, mut acks) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move { agent.run(Some(acks_tx)).await });
    timeout(Duration::from_secs(10), async {
        for _ in 0..3 {
            acks.recv().await.unwrap();
        }
    })
    .await
    .expect("acks");

    let agent = &agents(&api, session).await[0];
    assert_eq!(agent["id"], credential.agent_id.as_str());
    assert_eq!(agent["cpu_percent"], 42.5);
    assert_eq!(agent["mem_used_bytes"], 6u64 << 30);
    assert_eq!(agent["mem_total_bytes"], 16u64 << 30);
    assert_eq!(agent["disk_used_bytes"], 120u64 << 30);
    assert_eq!(agent["disk_total_bytes"], 500u64 << 30);
    // The latest sample wins.
    assert!(agent["uptime_secs"].as_u64().unwrap() >= 1002, "{agent}");
    assert!(!agent["telemetry_at"].is_null());
}

#[tokio::test]
async fn heartbeat_without_telemetry_only_updates_last_seen() {
    let db = start_db().await;
    support::insert_agent(&db.pool, "agent-1").await;
    server::registry::touch(&db.pool, "agent-1", None)
        .await
        .unwrap();
    let agent = &server::registry::list_agents(&db.pool).await.unwrap()[0];
    assert!(agent.last_seen.is_some());
    assert!(agent.telemetry_at.is_none() && agent.cpu_percent.is_none());

    // Out-of-range values from a misbehaving agent are clamped, not rejected.
    server::registry::touch(
        &db.pool,
        "agent-1",
        Some(&Telemetry {
            cpu_percent: 250.0,
            mem_used_bytes: u64::MAX,
            mem_total_bytes: 1,
            disk_used_bytes: 0,
            disk_total_bytes: 0,
            uptime_secs: 0,
        }),
    )
    .await
    .unwrap();
    let agent = &server::registry::list_agents(&db.pool).await.unwrap()[0];
    assert_eq!(agent.cpu_percent, Some(100.0));
    assert_eq!(agent.mem_used_bytes, Some(i64::MAX));
}
