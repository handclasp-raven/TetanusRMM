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
//! | GET    | /api/audit?limit=           | session (admin, auditor) |
//! | GET    | /api/audit/verify           | session (admin, auditor) |
//! | GET    | /api/agents/{id}/policy     | session         |
//! | PUT    | /api/agents/{id}/policy     | session (admin) |
//! | POST   | /api/enrollment-links       | session (admin, support_engineer) |
//! | POST   | /api/agents/{id}/viewer-sessions | session (admin, support_engineer) |
//! | GET    | /api/agents/{id}/shell?cols=&rows= | session (admin, support_engineer); WebSocket |
//! | POST   | /api/script-runs            | session (admin, support_engineer) |
//! | PUT    | /api/agents/{id}/files?path=&overwrite= | session (admin, support_engineer) |
//! | GET    | /api/agents/{id}/files?path= | session (admin, support_engineer) |
//! | GET    | /api/download/{platform}?token= | enrollment token |
//! | GET    | /api/updates/{platform}/manifest | none (content is signed) |
//! | GET    | /api/updates/{platform}/binary   | none (content is signed) |
//! | GET    | /api/updates/{platform}/signature| none |
//!
//! Sessions are passed as `Authorization: Bearer <token>`.

use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use protocol::shell::TermSize;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use tracing::error;

use crate::audit;
use crate::auth::{self, AuthSettings, LoginError};
use crate::enroll;
use crate::registry::{self, Agent, DevicePolicy, PolicyError};
use crate::remote::{self, files, script, shell, RemoteError};
use crate::updates;
use crate::users::User;
use crate::viewers::{self, ViewerError};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: AuthSettings,
    /// Base URL of this API, without a trailing slash, for building links.
    pub public_url: String,
    /// Where published agent updates live (see `crate::updates`).
    pub updates_dir: PathBuf,
    /// The media relay, to report whether an agent is online.
    pub hub: Option<Arc<crate::relay::Hub>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(login))
        .route("/api/auth/totp", post(login_totp))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/agents", get(list_agents))
        .route("/api/audit", get(list_audit))
        .route("/api/audit/verify", get(verify_audit))
        .route("/api/agents/{id}/policy", get(get_policy).put(put_policy))
        .route("/api/enrollment-links", post(create_enrollment_link))
        .route(
            "/api/agents/{id}/viewer-sessions",
            post(create_viewer_session),
        )
        .route("/api/agents/{id}/shell", get(open_shell))
        .route("/api/script-runs", post(run_script))
        .route(
            "/api/agents/{id}/files",
            get(download_file)
                .put(upload_file)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/download/{platform}", get(download_agent))
        .route("/api/updates/{platform}/manifest", get(update_manifest))
        .route("/api/updates/{platform}/binary", get(update_binary))
        .route("/api/updates/{platform}/signature", get(update_signature))
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
    /// Any other status, with a message for the client.
    #[error("{1}")]
    Status(StatusCode, String),
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
            ApiError::Status(status, _) => *status,
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl From<RemoteError> for ApiError {
    fn from(e: RemoteError) -> Self {
        use protocol::transfer::ErrorKind;
        let status = match &e {
            RemoteError::Forbidden => return ApiError::Forbidden,
            RemoteError::Db(e) => return ApiError::Internal(e.to_string()),
            RemoteError::BadRequest(_) => StatusCode::BAD_REQUEST,
            RemoteError::AgentOffline | RemoteError::Unsupported => StatusCode::CONFLICT,
            RemoteError::Agent { kind, .. } => match kind {
                ErrorKind::InvalidPath => StatusCode::BAD_REQUEST,
                ErrorKind::NotFound => StatusCode::NOT_FOUND,
                ErrorKind::AlreadyExists => StatusCode::CONFLICT,
                ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                ErrorKind::Integrity => StatusCode::UNPROCESSABLE_ENTITY,
                ErrorKind::Io => StatusCode::BAD_GATEWAY,
            },
            RemoteError::Transport(_) => StatusCode::BAD_GATEWAY,
        };
        ApiError::Status(status, e.to_string())
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

/// An agent as listed by the API: the registry row plus live state from
/// the relay hub.
#[derive(Serialize)]
struct AgentView {
    #[serde(flatten)]
    agent: Agent,
    /// Connected to the server right now.
    online: bool,
    /// Remote-desktop viewers watching it.
    viewer_sessions: usize,
    /// Interactive shells open on it.
    shell_sessions: usize,
}

async fn list_agents(
    State(state): State<AppState>,
    _session: Session,
) -> Result<Json<Vec<AgentView>>, ApiError> {
    let agents = registry::list_agents(&state.pool).await?;
    Ok(Json(
        agents
            .into_iter()
            .map(|agent| {
                let link = state.hub.as_ref().and_then(|hub| hub.get(&agent.id));
                AgentView {
                    online: link.is_some(),
                    viewer_sessions: link.as_ref().map_or(0, |l| l.viewers()),
                    shell_sessions: link.as_ref().map_or(0, |l| l.shells()),
                    agent,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct AuditQuery {
    /// Defaults to 100; at most 1000.
    limit: Option<i64>,
}

/// The newest audit entries, newest first.
async fn list_audit(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<AuditQuery>,
) -> Result<Json<Vec<audit::Entry>>, ApiError> {
    if !session.user.role.can_read_audit() {
        return Err(ApiError::Forbidden);
    }
    let limit = query.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::BadRequest(
            "limit must be between 1 and 1000".into(),
        ));
    }
    Ok(Json(audit::recent(&state.pool, limit).await?))
}

/// Walk the whole audit chain and report whether it is intact.
async fn verify_audit(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<audit::Verification>, ApiError> {
    if !session.user.role.can_read_audit() {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(audit::verify(&state.pool).await?))
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

#[derive(Deserialize)]
struct EnrollmentLinkRequest {
    /// Token lifetime; defaults to 24 h, capped at 7 days.
    ttl_secs: Option<u64>,
    /// Which agent build the link downloads; defaults to `windows-x86_64`.
    platform: Option<String>,
}

#[derive(Serialize)]
struct EnrollmentLinkResponse {
    token: String,
    expires_at: DateTime<Utc>,
    download_url: String,
}

async fn create_enrollment_link(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<EnrollmentLinkRequest>,
) -> Result<Json<EnrollmentLinkResponse>, ApiError> {
    if !session.user.role.can_create_enrollment_links() {
        return Err(ApiError::Forbidden);
    }
    let platform = req.platform.unwrap_or_else(|| "windows-x86_64".to_owned());
    if !protocol::update::valid_platform(&platform) {
        return Err(ApiError::BadRequest("invalid platform".into()));
    }
    let ttl = match req.ttl_secs {
        None => enroll::DEFAULT_TOKEN_TTL,
        Some(secs) if (1..=enroll::MAX_TOKEN_TTL.as_secs()).contains(&secs) => {
            Duration::from_secs(secs)
        }
        Some(_) => {
            return Err(ApiError::BadRequest(format!(
                "ttl_secs must be between 1 and {}",
                enroll::MAX_TOKEN_TTL.as_secs()
            )))
        }
    };
    let minted = enroll::create_token(&state.pool, &session.user.username, ttl).await?;
    Ok(Json(EnrollmentLinkResponse {
        download_url: format!(
            "{}/api/download/{platform}?token={}",
            state.public_url, minted.token
        ),
        token: minted.token,
        expires_at: minted.expires_at,
    }))
}

#[derive(Deserialize)]
struct DownloadQuery {
    token: String,
}

/// Download the latest agent build. Requires a usable (unused, unexpired)
/// enrollment token, but does not consume it: that happens at enrollment.
async fn download_agent(
    State(state): State<AppState>,
    Path(platform): Path<String>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    if !enroll::token_is_usable(&state.pool, &query.token).await? {
        return Err(ApiError::Unauthorized(
            "download link is invalid or expired",
        ));
    }
    let filename = if platform.starts_with("windows") {
        "rmm-agent.exe"
    } else {
        "rmm-agent"
    };
    let mut resp = serve_update_file(&state.updates_dir, &platform, updates::BINARY_FILE).await?;
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .expect("valid header"),
    );
    Ok(resp)
}

async fn update_manifest(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Result<Json<protocol::update::UpdateManifest>, ApiError> {
    updates::load_manifest(&state.updates_dir, &platform)
        .map_err(|e| match e {
            updates::UpdateError::InvalidPlatform(_) => ApiError::NotFound,
            other => ApiError::Internal(other.to_string()),
        })?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

async fn update_binary(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Result<Response, ApiError> {
    serve_update_file(&state.updates_dir, &platform, updates::BINARY_FILE).await
}

async fn update_signature(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Result<Response, ApiError> {
    serve_update_file(&state.updates_dir, &platform, updates::SIGNATURE_FILE).await
}

/// Stream a published file without loading it into memory.
async fn serve_update_file(dir: &FsPath, platform: &str, name: &str) -> Result<Response, ApiError> {
    if !protocol::update::valid_platform(platform) {
        return Err(ApiError::NotFound);
    }
    let path = dir.join(platform).join(name);
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError::NotFound),
        Err(e) => return Err(ApiError::Internal(format!("{}: {e}", path.display()))),
    };
    let len = file
        .metadata()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[derive(Serialize)]
struct ViewerSessionResponse {
    /// Pass to the viewer (`--token` or `RMM_VIEWER_TOKEN`). Single use.
    token: String,
    expires_at: DateTime<Utc>,
    agent_id: String,
    /// Whether the agent is connected right now.
    online: bool,
}

/// Mint a short-lived token for watching one agent's screen.
async fn create_viewer_session(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
) -> Result<Json<ViewerSessionResponse>, ApiError> {
    let minted = viewers::create(&state.pool, &session.user, &agent_id)
        .await
        .map_err(|e| match e {
            ViewerError::Forbidden => ApiError::Forbidden,
            ViewerError::UnknownAgent => ApiError::NotFound,
            ViewerError::InvalidToken => ApiError::Internal(e.to_string()),
            ViewerError::Db(e) => e.into(),
        })?;
    Ok(Json(ViewerSessionResponse {
        token: minted.token,
        expires_at: minted.expires_at,
        online: state.hub.as_ref().is_some_and(|h| h.is_online(&agent_id)),
        agent_id,
    }))
}

fn hub(state: &AppState) -> Result<&crate::relay::Hub, ApiError> {
    state.hub.as_deref().ok_or(ApiError::Unavailable)
}

#[derive(Deserialize)]
struct ShellQuery {
    cols: Option<u16>,
    rows: Option<u16>,
}

/// Interactive shell over a WebSocket (see `remote::shell`). The shell is
/// started before the upgrade, so failures are ordinary HTTP errors.
async fn open_shell(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Query(query): Query<ShellQuery>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let default = TermSize::default();
    let size = TermSize::new(
        query.cols.unwrap_or(default.cols),
        query.rows.unwrap_or(default.rows),
    )
    .ok_or_else(|| {
        ApiError::BadRequest(format!(
            "cols and rows must be between 1 and {}",
            protocol::shell::MAX_DIMENSION
        ))
    })?;
    if !session.user.role.can(crate::users::Capability::Shell) {
        // Audit the refusal even for a request that is not a WebSocket.
        remote::authorize(
            &state.pool,
            &session.user,
            crate::users::Capability::Shell,
            std::slice::from_ref(&agent_id),
        )
        .await?;
    }
    let upgrade = upgrade.map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let opened = shell::open(&state.pool, hub(&state)?, &session.user, &agent_id, size).await?;
    Ok(upgrade.on_upgrade(move |socket| shell::relay(socket, opened)))
}

#[derive(Deserialize)]
struct ScriptRunRequest {
    agent_ids: Vec<String>,
    /// PowerShell on Windows agents.
    script: String,
    /// Defaults to 300; at most 3600.
    timeout_secs: Option<u32>,
}

/// Run a script on one or more agents; returns every agent's result.
async fn run_script(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<ScriptRunRequest>,
) -> Result<Json<script::RunReport>, ApiError> {
    let agents = script::normalize_agents(&req.agent_ids)?;
    let request = protocol::script::ScriptRequest::new(req.script, req.timeout_secs)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let report = script::run(&state.pool, hub(&state)?, &session.user, agents, request).await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
struct FileQuery {
    /// Absolute path on the agent.
    path: String,
    #[serde(default)]
    overwrite: bool,
}

/// Header carrying a file's hex SHA-256: required on upload, set on download.
pub const SHA256_HEADER: &str = "x-content-sha256";

/// Upload the request body to `path` on the agent. Needs `Content-Length`
/// and `x-content-sha256`; the file appears only if both match.
async fn upload_file(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<files::Uploaded>, ApiError> {
    let size = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        .ok_or_else(|| {
            ApiError::Status(
                StatusCode::LENGTH_REQUIRED,
                "Content-Length is required".into(),
            )
        })?;
    let sha256 = headers
        .get(SHA256_HEADER)
        .and_then(|v| files::parse_sha256(v.to_str().ok()?))
        .ok_or_else(|| {
            ApiError::BadRequest(format!("{SHA256_HEADER} must be the file's hex SHA-256"))
        })?;
    let params = files::UploadParams {
        agent_id,
        path: query.path,
        overwrite: query.overwrite,
        size,
        sha256,
    };
    let uploaded = files::upload(
        &state.pool,
        hub(&state)?,
        &session.user,
        params,
        body.into_data_stream(),
    )
    .await?;
    Ok(Json(uploaded))
}

/// Download `path` from the agent, streamed. `x-content-sha256` is the
/// agent's hash of the file; a body that fails verification is cut short.
async fn download_file(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let download = files::download(
        &state.pool,
        hub(&state)?,
        &session.user,
        &agent_id,
        &query.path,
    )
    .await?;
    let name: String = query
        .path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("download")
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut resp = Body::from_stream(download.body).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, download.manifest.size.into());
    let hash = HeaderValue::from_str(&hex::encode(download.manifest.sha256))
        .expect("hex is a valid header");
    headers.insert(SHA256_HEADER, hash);
    if let Ok(disposition) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    Ok(resp)
}
