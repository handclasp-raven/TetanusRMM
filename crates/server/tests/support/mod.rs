//! Shared helpers for database-backed tests. Requires a running Docker daemon.

#![allow(dead_code)] // each test binary uses a different subset

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use common::devcerts::DevCerts;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use server::api::{self, AppState};
use server::auth::{totp, AuthSettings};
use server::enroll::AgentCa;
use server::users::{self, CreatedUser, Role};
use server::{Registry, Server, ServerConfig, ServerEvent};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

pub const PASSWORD: &str = "correct horse battery staple";

/// A throwaway Postgres with the schema migrated. The container is removed on drop.
pub struct TestDb {
    pub pool: PgPool,
    _container: ContainerAsync<Postgres>,
}

pub async fn start_db() -> TestDb {
    let db = start_db_unmigrated().await;
    server::db::MIGRATOR.run(&db.pool).await.expect("migrate");
    db
}

/// A throwaway Postgres with an empty schema, for migration tests.
pub async fn start_db_unmigrated() -> TestDb {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("starting Postgres container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("postgres port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    TestDb {
        pool,
        _container: container,
    }
}

pub async fn create_user(pool: &PgPool, username: &str, role: Role) -> CreatedUser {
    users::create_user(pool, "test", username, PASSWORD, role)
        .await
        .expect("create user")
}

/// The code an authenticator app would show right now.
pub fn current_code(secret: &str) -> String {
    totp::code_at(secret, totp::unix_now()).unwrap()
}

/// A well-formed code that is not valid now or in the next step.
pub fn wrong_code(secret: &str) -> String {
    let now = totp::unix_now();
    (0..1_000_000)
        .map(|n| format!("{n:06}"))
        .find(|c| {
            [now, now + 30]
                .iter()
                .all(|t| totp::check(secret, c, *t).unwrap().is_none())
        })
        .unwrap()
}

/// All audit actions, in chain order.
pub async fn audit_actions(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Register an enrolled agent directly, pinned to a dummy fingerprint.
pub async fn insert_agent(pool: &PgPool, agent_id: &str) {
    let mut tx = pool.begin().await.unwrap();
    server::registry::insert_enrolled(&mut tx, agent_id, "00")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// HTTPS client bound to a test API server.
pub struct Api {
    pub base: String,
    pub client: Client,
    _handle: ApiHandle,
}

struct ApiHandle(axum_server::Handle<SocketAddr>);

impl Drop for ApiHandle {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

impl Api {
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn login(&self, username: &str, code: &str) -> reqwest::Response {
        let challenge: Value = self
            .client
            .post(self.url("/api/auth/login"))
            .json(&json!({ "username": username, "password": PASSWORD }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        self.client
            .post(self.url("/api/auth/totp"))
            .json(&json!({ "challenge_token": challenge["challenge_token"], "code": code }))
            .send()
            .await
            .unwrap()
    }

    pub async fn session(&self, username: &str, secret: &str) -> String {
        let resp = self.login(username, &current_code(secret)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        body["session_token"].as_str().unwrap().to_owned()
    }
}

/// HTTPS API with fresh dev certs and no published updates.
pub async fn start_api(pool: PgPool) -> Api {
    let certs = common::devcerts::generate("unused").unwrap();
    start_api_with(pool, &certs, PathBuf::from("/nonexistent-updates")).await
}

/// HTTPS API presenting `certs`' server certificate and serving `updates_dir`.
pub async fn start_api_with(pool: PgPool, certs: &DevCerts, updates_dir: PathBuf) -> Api {
    start_api_full(pool, certs, updates_dir, None).await
}

/// [`start_api_with`], sharing the QUIC server's relay hub.
pub async fn start_api_full(
    pool: PgPool,
    certs: &DevCerts,
    updates_dir: PathBuf,
    hub: Option<Arc<server::relay::Hub>>,
) -> Api {
    let tls = common::tls::https_server_config(&certs.server_identity().unwrap()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let handle = axum_server::Handle::new();
    let app = api::router(AppState {
        pool,
        auth: AuthSettings::default(),
        public_url: base.clone(),
        updates_dir,
        hub,
    });
    tokio::spawn(api::serve(listener, tls, app, handle.clone()));

    // Client trusts only the dev CA, so this also proves the cert chain is right.
    let mut roots = rustls::RootCertStore::empty();
    for ca in certs.ca().unwrap() {
        roots.add(ca).unwrap();
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let client = Client::builder()
        .tls_backend_preconfigured(tls)
        .build()
        .unwrap();

    Api {
        base,
        client,
        _handle: ApiHandle(handle),
    }
}

/// QUIC listener with the registry and enrollment enabled.
pub fn start_quic(
    pool: PgPool,
    certs: &DevCerts,
) -> (Arc<Server>, SocketAddr, UnboundedReceiver<ServerEvent>) {
    let (tx, rx) = unbounded_channel();
    let quic = Server::bind(ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ws_listen: Some("127.0.0.1:0".parse().unwrap()),
        identity: certs.server_identity().unwrap(),
        client_ca: certs.ca().unwrap(),
    })
    .unwrap()
    .with_events(tx)
    .with_registry(Registry {
        pool,
        ca: AgentCa::from_pem(&certs.ca_cert, &certs.ca_key).unwrap(),
        api_url: "https://localhost:8443".into(),
    });
    let addr = quic.local_addr().unwrap();
    let quic = Arc::new(quic);
    let runner = quic.clone();
    tokio::spawn(async move { runner.run().await });
    (quic, addr, rx)
}
