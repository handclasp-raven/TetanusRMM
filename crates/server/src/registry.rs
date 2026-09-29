//! Agent registry and per-device consent policies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};

use crate::audit::{self, Action, NewEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "enrollment_state", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    Pending,
    Enrolled,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "consent_mode", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ConsentMode {
    Require,
    Notify,
    Unattended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "on_no_user", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum OnNoUser {
    Deny,
    Allow,
}

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Agent {
    pub id: String,
    pub enrollment_state: EnrollmentState,
    pub cert_fingerprint: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub cpu_percent: Option<f32>,
    pub mem_used_bytes: Option<i64>,
    pub mem_total_bytes: Option<i64>,
    pub disk_used_bytes: Option<i64>,
    pub disk_total_bytes: Option<i64>,
    pub uptime_secs: Option<i64>,
    pub telemetry_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct DevicePolicy {
    pub consent_mode: ConsentMode,
    pub on_no_user: OnNoUser,
    pub consent_timeout_secs: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("no such agent")]
    NotFound,
    #[error("consent_timeout_secs must be between 1 and 3600")]
    InvalidTimeout,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Register a newly enrolled agent, pinned to `cert_fingerprint`, with the
/// default device policy. Runs inside the enrollment transaction.
pub async fn insert_enrolled(
    tx: &mut Transaction<'_, Postgres>,
    agent_id: &str,
    cert_fingerprint: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO agents (id, enrollment_state, cert_fingerprint)
         VALUES ($1, 'enrolled', $2)",
    )
    .bind(agent_id)
    .bind(cert_fingerprint)
    .execute(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO device_policies (agent_id) VALUES ($1)")
        .bind(agent_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Check an agent's Hello against the registry and, if it passes, update
/// `last_seen`.
///
/// Passes only if `agent_id` is enrolled (not pending or revoked) and pinned
/// to exactly this certificate. A valid certificate from our CA is not
/// enough on its own: it must be the one issued to this agent.
pub async fn authenticate_hello(
    pool: &PgPool,
    agent_id: &str,
    cert_fingerprint: &str,
) -> sqlx::Result<bool> {
    let updated = sqlx::query(
        "UPDATE agents SET last_seen = now()
         WHERE id = $1 AND cert_fingerprint = $2 AND enrollment_state = 'enrolled'",
    )
    .bind(agent_id)
    .bind(cert_fingerprint)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Update `last_seen` on heartbeat and, if the heartbeat carried telemetry,
/// the latest health values and `telemetry_at`.
pub async fn touch(
    pool: &PgPool,
    agent_id: &str,
    telemetry: Option<&protocol::Telemetry>,
) -> sqlx::Result<()> {
    match telemetry {
        None => {
            sqlx::query("UPDATE agents SET last_seen = now() WHERE id = $1")
                .bind(agent_id)
                .execute(pool)
                .await?;
        }
        Some(t) => {
            // BIGINT columns: u64 values beyond i64::MAX (not physically
            // possible for these quantities) are clamped rather than wrapped.
            let big = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
            sqlx::query(
                "UPDATE agents SET last_seen = now(), telemetry_at = now(),
                     cpu_percent = $2, mem_used_bytes = $3, mem_total_bytes = $4,
                     disk_used_bytes = $5, disk_total_bytes = $6, uptime_secs = $7
                 WHERE id = $1",
            )
            .bind(agent_id)
            .bind(t.cpu_percent.clamp(0.0, 100.0))
            .bind(big(t.mem_used_bytes))
            .bind(big(t.mem_total_bytes))
            .bind(big(t.disk_used_bytes))
            .bind(big(t.disk_total_bytes))
            .bind(big(t.uptime_secs))
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

pub async fn list_agents(pool: &PgPool) -> sqlx::Result<Vec<Agent>> {
    sqlx::query_as("SELECT * FROM agents ORDER BY id")
        .fetch_all(pool)
        .await
}

pub async fn get_policy(pool: &PgPool, agent_id: &str) -> sqlx::Result<Option<DevicePolicy>> {
    sqlx::query_as(
        "SELECT consent_mode, on_no_user, consent_timeout_secs
         FROM device_policies WHERE agent_id = $1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
}

/// Replace an agent's policy. Audited as `policy.update` with before and after.
pub async fn update_policy(
    pool: &PgPool,
    actor: &str,
    agent_id: &str,
    policy: DevicePolicy,
) -> Result<DevicePolicy, PolicyError> {
    if !(1..=3600).contains(&policy.consent_timeout_secs) {
        return Err(PolicyError::InvalidTimeout);
    }
    let mut tx = pool.begin().await?;
    let before: Option<DevicePolicy> = sqlx::query_as(
        "SELECT consent_mode, on_no_user, consent_timeout_secs
         FROM device_policies WHERE agent_id = $1 FOR UPDATE",
    )
    .bind(agent_id)
    .fetch_optional(&mut *tx)
    .await?;
    let before = before.ok_or(PolicyError::NotFound)?;

    sqlx::query(
        "UPDATE device_policies
         SET consent_mode = $2, on_no_user = $3, consent_timeout_secs = $4, updated_at = now()
         WHERE agent_id = $1",
    )
    .bind(agent_id)
    .bind(policy.consent_mode)
    .bind(policy.on_no_user)
    .bind(policy.consent_timeout_secs)
    .execute(&mut *tx)
    .await?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::PolicyUpdate)
            .target(agent_id)
            .detail(json!({ "before": before, "after": policy })),
    )
    .await?;
    tx.commit().await?;
    Ok(policy)
}
