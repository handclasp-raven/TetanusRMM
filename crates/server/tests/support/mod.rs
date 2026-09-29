//! Shared helpers for database-backed tests. Requires a running Docker daemon.

#![allow(dead_code)] // each test binary uses a different subset

use server::auth::totp;
use server::users::{self, CreatedUser, Role};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

pub const PASSWORD: &str = "correct horse battery staple";

/// A throwaway Postgres with the schema migrated. The container is removed on drop.
pub struct TestDb {
    pub pool: PgPool,
    _container: ContainerAsync<Postgres>,
}

pub async fn start_db() -> TestDb {
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
    let pool = server::db::connect(&url, 5)
        .await
        .expect("connect + migrate");
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
