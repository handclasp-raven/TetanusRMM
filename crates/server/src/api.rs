//! HTTPS API for users.
//!
//! | Method | Path                        | Auth            |
//! |--------|-----------------------------|-----------------|
//! | GET    | /api/health                 | none            |
//! | POST   | /api/auth/login             | none            |
//! | POST   | /api/auth/totp              | challenge token |
//! | POST   | /api/auth/logout            | session         |
//! | GET    | /api/me                     | session         |
//! | GET    | /api/agents                 | session         |
//! | GET    | /api/agents/{id}/policy     | session         |
//! | PUT    | /api/agents/{id}/policy     | session (admin) |
//!
//! Sessions are passed as `Authorization: Bearer <token>`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use tracing::error;

use crate::auth::{self, AuthSettings, LoginError};
use crate::registry::{self, Agent, DevicePolicy, PolicyError};
use crate::users::User;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: AuthSettings,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(login))
        .route("/api/auth/totp", post(login_totp))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}/policy", get(get_policy).put(put_policy))
        .with_state(state)
}

/// Serve `app` over HTTPS on an already-bound listener until `handle` is shut down.
pub async fn serve(
    listener: std::net::TcpListener,
    tls: rustls::ServerConfig,
    app: Router,
    handle: axum_server::Handle<SocketAddr>,
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls));
    axum_server::from_tcp_rustls(listener, tls)?
        .handle(handle)
        .serve(app.into_make_service())
        .await
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(&'static str),
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("service unavailable")]
    Unavailable,
    #[error("internal error")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(detail) => {
                error!("internal error: {detail}");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl From<LoginError> for ApiError {
    fn from(e: LoginError) -> Self {
        match e {
            LoginError::InvalidCredentials => {
                ApiError::Unauthorized("invalid username or password")
            }
            LoginError::InvalidChallenge => {
                ApiError::Unauthorized("login challenge is invalid or expired")
            }
            LoginError::InvalidTotp => ApiError::Unauthorized("invalid TOTP code"),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

/// An authenticated user, extracted from the bearer token.
pub struct Session {
    pub user: User,
    pub token: String,
}

impl FromRequestParts<AppState> for Session {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(ApiError::Unauthorized("missing bearer token"))?;
        let user = auth::authenticate(&state.pool, token)
            .await?
            .ok_or(ApiError::Unauthorized("invalid or expired session"))?;
        Ok(Session {
            user,
            token: token.to_owned(),
        })
    }
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    sqlx::query("SELECT 1")
        .execute(&state.pool)
        .await
        .map_err(|_| ApiError::Unavailable)?;
    Ok(Json(json!({ "status": "ok" })))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    challenge_token: String,
    expires_in_secs: u64,
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let challenge = auth::login_password(&state.pool, &req.username, &req.password).await?;
    Ok(Json(LoginResponse {
        challenge_token: challenge.challenge_token,
        expires_in_secs: challenge.expires_in.as_secs(),
    }))
}

#[derive(Deserialize)]
struct TotpRequest {
    challenge_token: String,
    code: String,
}

#[derive(Serialize)]
struct TotpResponse {
    session_token: String,
    expires_at: DateTime<Utc>,
    user: User,
}

async fn login_totp(
    State(state): State<AppState>,
    Json(req): Json<TotpRequest>,
) -> Result<Json<TotpResponse>, ApiError> {
    let grant = auth::login_totp(&state.pool, &state.auth, &req.challenge_token, &req.code).await?;
    Ok(Json(TotpResponse {
        session_token: grant.token,
        expires_at: grant.expires_at,
        user: grant.user,
    }))
}

async fn logout(State(state): State<AppState>, session: Session) -> Result<StatusCode, ApiError> {
    auth::logout(&state.pool, &session.user, &session.token).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn me(session: Session) -> Json<User> {
    Json(session.user)
}

async fn list_agents(
    State(state): State<AppState>,
    _session: Session,
) -> Result<Json<Vec<Agent>>, ApiError> {
    Ok(Json(registry::list_agents(&state.pool).await?))
}

async fn get_policy(
    State(state): State<AppState>,
    _session: Session,
    Path(agent_id): Path<String>,
) -> Result<Json<DevicePolicy>, ApiError> {
    registry::get_policy(&state.pool, &agent_id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

async fn put_policy(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Json(policy): Json<DevicePolicy>,
) -> Result<Json<DevicePolicy>, ApiError> {
    if !session.user.role.can_edit_policies() {
        return Err(ApiError::Forbidden);
    }
    registry::update_policy(&state.pool, &session.user.username, &agent_id, policy)
        .await
        .map(Json)
        .map_err(|e| match e {
            PolicyError::NotFound => ApiError::NotFound,
            PolicyError::InvalidTimeout => ApiError::BadRequest(e.to_string()),
            PolicyError::Db(e) => e.into(),
        })
}
