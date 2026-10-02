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

    fn signed_in_users(&self) -> Vec<String> {
        vec!["CORP\\alice".into(), "bob\u{7}".into()]
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
        transport: Default::default(),
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
    let before = &agents(&api, session.clone()).await[0];
    assert!(before["cpu_percent"].is_null());
    assert!(before["logged_in_users"].is_null() && before["remote_ip"].is_null());

    let mut config = credential.agent_config(Duration::from_millis(50)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.telemetry = Some(Arc::new(Fixed {
        samples: AtomicU64::new(0),
    }));
    let agent = agent::connect(&config).await.unwrap();
    let (acks_tx, mut acks) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move { agent.run(Some(acks_tx)).await });
    timeout(Duration::from_secs(10), async {
        // Count heartbeat acks only (the agent also sees ListMonitors).
        let mut n = 0;
        while n < 3 {
            if let agent::AgentEvent::Ack { .. } = acks.recv().await.unwrap() {
                n += 1;
            }
        }
    })
    .await
    .expect("acks");

    // Status arrives on its own schedule, right after the server asks.
    let agent = timeout(Duration::from_secs(10), async {
        loop {
            let agent = agents(&api, session.clone()).await.remove(0);
            if !agent["logged_in_users"].is_null() && !agent["remote_ip"].is_null() {
                return agent;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("status reported");
    // Cleaned by the server.
    assert_eq!(
        agent["logged_in_users"],
        serde_json::json!(["CORP\\alice", "bob"])
    );
    assert_eq!(agent["local_ip"], "127.0.0.1");
    assert_eq!(agent["remote_ip"], "127.0.0.1");
    // This machine's OS, as the agent describes it.
    assert!(
        !agent["os"].as_str().expect("os reported").is_empty(),
        "{agent}"
    );
    // No admin has classified it: the reported device kind decides.
    assert!(agent["classification_override"].is_null());
    let expected = match agent["device_kind"].as_str() {
        Some("server") => "server",
        Some("workstation") => "desktop",
        _ => "other",
    };
    assert_eq!(agent["classification"], expected);
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

fn sample(cpu_percent: f32) -> Telemetry {
    Telemetry {
        cpu_percent,
        mem_used_bytes: 6 << 30,
        mem_total_bytes: 16 << 30,
        disk_used_bytes: 120 << 30,
        disk_total_bytes: 500 << 30,
        uptime_secs: 1000,
    }
}

/// A history sample taken `minutes_ago`, as an earlier heartbeat left it.
async fn insert_sample(pool: &sqlx::PgPool, agent_id: &str, minutes_ago: i32, cpu_percent: f32) {
    sqlx::query(
        "INSERT INTO agent_telemetry (agent_id, ts, cpu_percent, mem_used_bytes,
             mem_total_bytes, disk_used_bytes, disk_total_bytes)
         VALUES ($1, now() - make_interval(mins => $2), $3, 1, 2, 3, 4)",
    )
    .bind(agent_id)
    .bind(minutes_ago)
    .bind(cpu_percent)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn telemetry_history_keeps_one_sample_per_step_and_is_pruned() {
    let db = start_db().await;
    support::insert_agent(&db.pool, "agent-1").await;
    let long_ago = chrono::Utc::now() - chrono::Duration::days(30);

    // Heartbeats arrive every few seconds: only the first of a step is kept.
    for cpu in [10.0, 20.0, 30.0] {
        server::registry::touch(&db.pool, "agent-1", Some(&sample(cpu)))
            .await
            .unwrap();
    }
    let history = server::registry::telemetry_history(&db.pool, "agent-1", long_ago)
        .await
        .unwrap();
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0].cpu_percent, 10.0);
    assert_eq!(history[0].mem_total_bytes, 16 << 30);
    // The agents row still has the latest.
    let agent = &server::registry::list_agents(&db.pool).await.unwrap()[0];
    assert_eq!(agent.cpu_percent, Some(30.0));

    // A heartbeat from an agent that is not registered leaves nothing behind.
    server::registry::touch(&db.pool, "agent-gone", Some(&sample(1.0)))
        .await
        .unwrap();

    insert_sample(&db.pool, "agent-1", 10, 5.0).await;
    insert_sample(&db.pool, "agent-1", 3 * 24 * 60, 99.0).await;
    let pruned = server::registry::prune_telemetry(&db.pool, server::registry::TELEMETRY_RETENTION)
        .await
        .unwrap();
    assert_eq!(pruned, 1);
    let history = server::registry::telemetry_history(&db.pool, "agent-1", long_ago)
        .await
        .unwrap();
    let cpu: Vec<f32> = history.iter().map(|s| s.cpu_percent).collect();
    assert_eq!(cpu, [5.0, 10.0]);
}

#[tokio::test]
async fn telemetry_history_is_served_to_those_who_see_the_agent() {
    let db = start_db().await;
    support::insert_agent(&db.pool, "agent-1").await;
    insert_sample(&db.pool, "agent-1", 90, 1.0).await;
    insert_sample(&db.pool, "agent-1", 20, 2.0).await;
    insert_sample(&db.pool, "agent-1", 5, 3.0).await;
    let auditor = create_user(&db.pool, "root", Role::Auditor).await;
    let engineer = create_user(&db.pool, "eng", Role::SupportEngineer).await;
    let api = support::start_api(db.pool.clone()).await;
    let session = api.session("root", &auditor.totp_secret).await;
    let get = |query: &str, session: &str| {
        let req = api
            .client
            .get(api.url(&format!("/api/agents/agent-1/telemetry{query}")))
            .bearer_auth(session);
        async move { req.send().await.unwrap() }
    };
    let cpu = |body: &Value| -> Vec<f64> {
        body["samples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["cpu_percent"].as_f64().unwrap())
            .collect()
    };

    // The last hour by default, oldest first.
    let body: Value = get("", &session).await.json().await.unwrap();
    assert_eq!(body["step_secs"], 30);
    assert_eq!(cpu(&body), [2.0, 3.0]);
    assert_eq!(body["samples"][0]["mem_total_bytes"], 2);
    let body: Value = get("?minutes=120", &session).await.json().await.unwrap();
    assert_eq!(cpu(&body), [1.0, 2.0, 3.0]);

    // Only what is newer than the samples already fetched.
    let since = body["samples"][1]["ts"]
        .as_str()
        .unwrap()
        .replace('+', "%2B");
    let body: Value = get(&format!("?since={since}"), &session)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(cpu(&body), [3.0]);

    assert_eq!(get("?minutes=0", &session).await.status(), 400);
    assert_eq!(get("", "not-a-session").await.status(), 401);
    // An engineer with no grant on the agent cannot tell that it exists.
    let session = api.session("eng", &engineer.totp_secret).await;
    assert_eq!(get("", &session).await.status(), 404);
}
