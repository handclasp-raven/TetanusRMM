//! Deployment keys (see `crate::enroll`): reusable enrollment keys, each
//! with an MSI to push to many machines with Group Policy or Intune.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{install_target, rbac, ApiError, AppState, Session, MSI_PLATFORM};
use crate::enroll::{self, DeploymentKey};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/deployment-keys", get(list).post(create))
        .route("/api/deployment-keys/{id}", delete(revoke))
}

#[derive(Deserialize)]
struct NewKeyRequest {
    /// What the key is for, e.g. the customer or site.
    name: String,
    /// Lifetime, up to a year; absent or null for a key that never expires.
    ttl_secs: Option<u64>,
    /// Groups each agent joins when it enrolls (admins only).
    #[serde(default)]
    group_ids: Vec<i64>,
    /// As for `/api/enrollment-links`.
    server: Option<String>,
    server_name: Option<String>,
}

#[derive(Serialize)]
struct NewKeyResponse {
    id: i64,
    name: String,
    /// The key. Shown once: it is not stored, so neither it nor the MSI
    /// can be fetched again later without it.
    token: String,
    expires_at: Option<DateTime<Utc>>,
    /// An MSI that installs the agent unattended and enrolls it with the key.
    msi_url: String,
    server: String,
    server_name: String,
}

async fn create(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<NewKeyRequest>,
) -> Result<Json<NewKeyResponse>, ApiError> {
    if !session.user.role.can_create_enrollment_links() {
        return Err(ApiError::Forbidden);
    }
    let name = req.name.trim();
    if name.is_empty()
        || name.chars().count() > enroll::MAX_DEPLOYMENT_KEY_NAME
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::BadRequest(format!(
            "name must be 1 to {} characters",
            enroll::MAX_DEPLOYMENT_KEY_NAME
        )));
    }
    let ttl = match req.ttl_secs {
        None => None,
        Some(secs) if (1..=enroll::MAX_DEPLOYMENT_KEY_TTL.as_secs()).contains(&secs) => {
            Some(Duration::from_secs(secs))
        }
        Some(_) => {
            return Err(ApiError::BadRequest(format!(
                "ttl_secs must be between 1 and {}, or null for no expiry",
                enroll::MAX_DEPLOYMENT_KEY_TTL.as_secs()
            )))
        }
    };
    if !req.group_ids.is_empty() {
        // Group membership extends grants: only admins decide it.
        rbac::require_admin(&state.pool, &session.user, "deployment_key_groups").await?;
        crate::groups::check_exist(&state.pool, &req.group_ids)
            .await
            .map_err(rbac::group_error)?;
    }
    let install = install_target(
        &state.public_url,
        req.server.as_deref(),
        req.server_name.as_deref(),
    )?;
    let (key, token) = enroll::create_deployment_key(
        &state.pool,
        &session.user.username,
        name,
        ttl,
        &req.group_ids,
        &install,
    )
    .await?;
    Ok(Json(NewKeyResponse {
        msi_url: format!(
            "{}/api/download/{MSI_PLATFORM}/msi?token={token}",
            state.public_url
        ),
        id: key.id,
        name: key.name,
        token,
        expires_at: key.expires_at,
        server: key.server,
        server_name: key.server_name,
    }))
}

async fn list(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<Vec<DeploymentKey>>, ApiError> {
    if !session.user.role.can_create_enrollment_links() {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(enroll::list_deployment_keys(&state.pool).await?))
}

/// Revoke a key. Agents that enrolled with it are not affected.
async fn revoke(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<Json<DeploymentKey>, ApiError> {
    if !session.user.role.can_create_enrollment_links() {
        return Err(ApiError::Forbidden);
    }
    enroll::revoke_deployment_key(&state.pool, &session.user.username, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}
