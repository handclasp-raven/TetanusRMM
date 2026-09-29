//! Postgres connection pool and migrations.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Migrations from `crates/server/migrations`, embedded in the binary.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("connecting to database: {0}")]
    Connect(#[source] sqlx::Error),
    #[error("running migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Open a bounded pool and bring the schema up to date.
pub async fn connect(database_url: &str, max_connections: u32) -> Result<PgPool, DbError> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .map_err(DbError::Connect)?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}
