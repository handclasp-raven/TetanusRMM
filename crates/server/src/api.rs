//! HTTPS API for users.
//!
//! | Method | Path                        | Auth            |
//! |--------|-----------------------------|-----------------|
//! | GET    | /api/health                 | none            |
//! | POST   | /api/auth/login             | none            |
//! | POST   | /api/auth/totp              | challenge token |
//! | POST   | /api/auth/logout            | session         |
//! | GET    | /api/me                     | session         |
//! | GET    | /api/agents                 | session (engineers: granted agents only) |
//! | GET    | /api/agents/{id}            | session (agent visible to the user) |
//! | GET    | /api/audit?limit=           | session (admin, auditor) |
//! | GET    | /api/audit/verify           | session (admin, auditor) |
//! | GET    | /api/agents/{id}/policy     | session (agent visible to the user) |
//! | PUT    | /api/agents/{id}/policy     | session (admin) |
//! | PUT    | /api/agents/{id}/classification | session (admin) |
//! | POST   | /api/agents/{id}/launch     | `desktop` on the agent |
//! | POST   | /api/enrollment-links       | session (admin, support_engineer; `group_ids` admin only) |
//! | POST   | /api/agents/{id}/viewer-sessions | `desktop` on the agent |
//! | GET    | /api/agents/{id}/shell?cols=&rows= | `shell` on the agent; WebSocket |
//! | POST   | /api/script-runs            | `script` on every target agent |
//! | PUT    | /api/agents/{id}/files?path=&overwrite= | `file_transfer` on the agent |
//! | GET    | /api/agents/{id}/files?path= | `file_transfer` on the agent |
//! | GET    | /api/users                  | session (admin) |
//! | POST   | /api/users                  | session (admin) |
//! | PUT    | /api/users/{id}/role        | session (admin) |
//! | PUT    | /api/users/{id}/password    | session (admin) |
//! | POST   | /api/users/{id}/totp        | session (admin) |
//! | DELETE | /api/users/{id}             | session (admin) |
//! | GET    | /api/grants?user_id=        | session (admin; anyone for their own) |
//! | POST   | /api/grants                 | session (admin) |
//! | DELETE | /api/grants/{id}            | session (admin) |
//! | GET    | /api/groups                 | session |
//! | POST   | /api/groups                 | session (admin) |
//! | GET    | /api/groups/{id}            | session |
//! | PATCH  | /api/groups/{id}            | session (admin) |
//! | DELETE | /api/groups/{id}            | session (admin) |
//! | PUT    | /api/groups/{id}/agents     | session (admin): set members |
//! | POST   | /api/groups/{id}/agents     | session (admin): add members |
//! | DELETE | /api/groups/{id}/agents/{agent_id} | session (admin) |
//! | GET    | /api/download/{platform}?token= | enrollment token |
//! | GET    | /api/updates/{platform}/manifest | none (content is signed) |
//! | GET    | /api/updates/{platform}/binary   | none (content is signed) |
//! | GET    | /api/updates/{platform}/signature| none |
//! | GET    | /api/updates/pubkey         | none (agents pin it on first use) |
//! | GET    | /api/ca                     | none (the CA certificate is public) |
//! | GET    | /api/viewer/{platform}/manifest | none |
//! | GET    | /api/viewer/{platform}/binary   | none |
//! | POST   | /api/assist-sessions        | session (admin, support_engineer): a quick assist code |
//! | GET    | /api/assist-sessions/{id}   | session (whoever made it; admin) |
//! | GET    | /assist                     | none: the quick assist page (`assist`) |
//! | GET    | /assist/download            | none: the quick assist client, set up for this server |
//! | GET    | /install                    | none: the staff install page (`install`) |
//! | GET    | /install/{wheel}            | none: the published TUI wheel |
//! | GET    | /install/viewer/{platform}  | none: the published viewer |
//!
//! Sessions are passed as `Authorization: Bearer <token>`.
//!
//! "`capability` on the agent" means the user's role allows it and, for
//! support engineers, one of their grants covers that agent (see
//! `crate::access`). Refusals of those are audited as `permission.denied`,
//! as are non-admins' attempts at the admin-only endpoints above.

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
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use protocol::shell::TermSize;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use tracing::error;

mod assist;
mod install;
mod rbac;

use crate::access;
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
    /// PEM CA certificate agents must trust (it signed the server's
    /// certificate), packaged into MSIs and served at `/api/ca`.
    pub server_ca_pem: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(login))
        .route("/api/auth/totp", post(login_totp))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}", get(get_agent))
        .route("/api/agents/{id}/telemetry", get(agent_telemetry))
        .route("/api/agents/{id}/launch", post(launch_command))
        .route("/api/audit", get(list_audit))
        .route("/api/audit/verify", get(verify_audit))
        .route("/api/agents/{id}/policy", get(get_policy).put(put_policy))
        .route("/api/agents/{id}/classification", put(put_classification))
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
        .route("/api/download/{platform}/msi", get(download_msi))
        .route("/api/updates/{platform}/manifest", get(update_manifest))
        .route("/api/updates/{platform}/binary", get(update_binary))
        .route("/api/updates/{platform}/signature", get(update_signature))
        .route("/api/updates/pubkey", get(update_public_key))
        .route("/api/ca", get(ca_certificate))
        .route("/api/viewer/{platform}/manifest", get(viewer_manifest))
        .route("/api/viewer/{platform}/binary", get(viewer_binary))
        .merge(rbac::routes())
        .merge(assist::routes())
        .merge(install::routes())
        // Per route, so the route template is known (unmatched requests
        // are not counted).
        .route_layer(axum::middleware::from_fn(crate::metrics::track_http))
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
            RemoteError::AgentOffline | RemoteError::Unsupported | RemoteError::NoUser => {
                StatusCode::CONFLICT
            }
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

fn count_login(result: &'static str) {
    crate::metrics::get()
        .logins
        .get_or_create(&crate::metrics::ResultLabels { result })
        .inc();
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let challenge = auth::login_password(&state.pool, &req.username, &req.password)
        .await
        .inspect_err(|e| {
            if matches!(e, LoginError::InvalidCredentials) {
                count_login("bad_password");
            }
        })?;
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
    let grant = auth::login_totp(&state.pool, &state.auth, &req.challenge_token, &req.code)
        .await
        .inspect_err(|e| match e {
            LoginError::InvalidTotp => count_login("bad_totp"),
            LoginError::InvalidChallenge => count_login("bad_challenge"),
            _ => {}
        })?;
    count_login("success");
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
    /// How it is connected (`quic`, or `websocket` when UDP is blocked);
    /// `null` when offline.
    transport: Option<transport::TransportKind>,
    /// Remote-desktop viewers watching it.
    viewer_sessions: usize,
    /// Interactive shells open on it.
    shell_sessions: usize,
    /// Names of the groups it is in.
    groups: Vec<String>,
    /// Server, desktop or other: an admin's choice
    /// (`classification_override`), else from the reported device kind.
    classification: registry::Classification,
    /// What the requesting user may do on it.
    capabilities: Vec<crate::users::Capability>,
}

/// The agents the user can see: all of them for admins and auditors, the
/// granted ones for support engineers.
async fn list_agents(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<Vec<AgentView>>, ApiError> {
    let visibility = access::visibility(&state.pool, &session.user).await?;
    let agents = registry::list_agents(&state.pool).await?;
    let mut groups = crate::groups::names_by_agent(&state.pool).await?;
    Ok(Json(
        agents
            .into_iter()
            .filter(|agent| visibility.sees(&agent.id))
            // A quick assist agent is listed only while it is there: it is
            // deleted soon after it goes.
            .filter(|agent| {
                agent.assist_session_id.is_none()
                    || state.hub.as_ref().is_some_and(|h| h.is_online(&agent.id))
            })
            .map(|agent| {
                let groups = groups.remove(&agent.id).unwrap_or_default();
                agent_view(&state, &visibility, agent, groups)
            })
            .collect(),
    ))
}

fn agent_view(
    state: &AppState,
    visibility: &access::Visibility,
    agent: Agent,
    groups: Vec<String>,
) -> AgentView {
    let link = state.hub.as_ref().and_then(|hub| hub.get(&agent.id));
    AgentView {
        groups,
        classification: agent.effective_classification(),
        capabilities: visibility
            .capabilities(&agent.id, agent.assist_session_id.is_some())
            .into_iter()
            .collect(),
        online: link.is_some(),
        transport: link.as_ref().and_then(|l| l.transport()),
        viewer_sessions: link.as_ref().map_or(0, |l| l.viewers()),
        shell_sessions: link.as_ref().map_or(0, |l| l.shells()),
        agent,
    }
}

/// One agent, as listed by `GET /api/agents` (the viewer's status panel).
async fn get_agent(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentView>, ApiError> {
    let visibility = access::visibility(&state.pool, &session.user).await?;
    // Agents the user cannot see do not exist, as far as they know.
    if !visibility.sees(&agent_id) {
        return Err(ApiError::NotFound);
    }
    let agent = registry::get_agent(&state.pool, &agent_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let groups = crate::groups::names_by_agent(&state.pool)
        .await?
        .remove(&agent_id)
        .unwrap_or_default();
    Ok(Json(agent_view(&state, &visibility, agent, groups)))
}

#[derive(Deserialize)]
struct TelemetryQuery {
    /// Only samples taken after this (to add to ones already fetched).
    since: Option<DateTime<Utc>>,
    /// Otherwise the last this many minutes. Defaults to 60.
    minutes: Option<i64>,
}

#[derive(Serialize)]
struct TelemetryHistory {
    /// Seconds between samples while the agent is connected.
    step_secs: u64,
    /// Oldest first.
    samples: Vec<registry::TelemetrySample>,
}

/// An agent's recent telemetry samples (the TUI's stats panel).
async fn agent_telemetry(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Query(query): Query<TelemetryQuery>,
) -> Result<Json<TelemetryHistory>, ApiError> {
    // Agents the user cannot see do not exist, as far as they know.
    if !access::visibility(&state.pool, &session.user)
        .await?
        .sees(&agent_id)
    {
        return Err(ApiError::NotFound);
    }
    let retention = i64::try_from(registry::TELEMETRY_RETENTION.as_secs() / 60).unwrap_or(i64::MAX);
    let minutes = query.minutes.unwrap_or(60);
    if !(1..=retention).contains(&minutes) {
        return Err(ApiError::BadRequest(format!(
            "minutes must be between 1 and {retention}"
        )));
    }
    let window = Utc::now() - chrono::Duration::minutes(minutes);
    let since = query.since.map_or(window, |since| since.max(window));
    Ok(Json(TelemetryHistory {
        step_secs: registry::TELEMETRY_STEP.as_secs(),
        samples: registry::telemetry_history(&state.pool, &agent_id, since).await?,
    }))
}

#[derive(Deserialize)]
struct LaunchBody {
    /// As typed at a Run prompt: `cmd`, `ncpa.cpl`, `mstsc /v:srv01`.
    command: String,
}

/// Start a program on the agent's desktop, as the signed-in user.
async fn launch_command(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Json(body): Json<LaunchBody>,
) -> Result<Json<remote::launch::Launched>, ApiError> {
    let launched = remote::launch::launch(
        &state.pool,
        hub(&state)?,
        &session.user,
        &agent_id,
        &body.command,
    )
    .await?;
    Ok(Json(launched))
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
    session: Session,
    Path(agent_id): Path<String>,
) -> Result<Json<DevicePolicy>, ApiError> {
    // Agents the user cannot see do not exist, as far as they know.
    if !access::visibility(&state.pool, &session.user)
        .await?
        .sees(&agent_id)
    {
        return Err(ApiError::NotFound);
    }
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
struct ClassificationRequest {
    /// `null` returns the agent to its derived classification.
    classification: Option<registry::Classification>,
}

#[derive(Serialize)]
struct ClassificationResponse {
    /// In effect now.
    classification: registry::Classification,
    /// What the admin chose, if anything.
    classification_override: Option<registry::Classification>,
}

/// Classify an agent as a server, a desktop or other (admins only).
async fn put_classification(
    State(state): State<AppState>,
    session: Session,
    Path(agent_id): Path<String>,
    Json(request): Json<ClassificationRequest>,
) -> Result<Json<ClassificationResponse>, ApiError> {
    rbac::require_admin(&state.pool, &session.user, "agent_classify").await?;
    let classification = registry::set_classification(
        &state.pool,
        &session.user.username,
        &agent_id,
        request.classification,
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(ClassificationResponse {
        classification,
        classification_override: request.classification,
    }))
}

#[derive(Deserialize)]
struct EnrollmentLinkRequest {
    /// Token lifetime; defaults to 24 h, capped at 7 days.
    ttl_secs: Option<u64>,
    /// Which agent build the link downloads; defaults to `windows-x86_64`.
    platform: Option<String>,
    /// Groups the agent joins when it enrolls (admins only).
    #[serde(default)]
    group_ids: Vec<i64>,
    /// Where an agent installed from the MSI connects: QUIC `host:port`.
    /// Defaults to this server's public host on port 4433.
    server: Option<String>,
    /// Name the server certificate must be valid for. Defaults to the
    /// host of `server` if it is a name, else this server's public host.
    server_name: Option<String>,
}

#[derive(Serialize)]
struct EnrollmentLinkResponse {
    token: String,
    expires_at: DateTime<Utc>,
    download_url: String,
    /// Windows only: an MSI that installs and enrolls the agent unattended.
    msi_url: Option<String>,
    /// What the MSI's agent connects to.
    server: String,
    server_name: String,
}

/// QUIC port agents use unless told otherwise.
const DEFAULT_AGENT_PORT: u16 = 4433;

/// The platform whose builds can be packaged as an MSI.
pub(crate) const MSI_PLATFORM: &str = "windows-x86_64";

/// The host part of this server's public URL.
fn public_host(public_url: &str) -> String {
    public_url
        .parse::<axum::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_owned))
        .unwrap_or_else(|| "localhost".into())
}

/// The link's install target: the request's, checked, with defaults.
fn install_target(
    public_url: &str,
    server: Option<&str>,
    server_name: Option<&str>,
) -> Result<enroll::InstallTarget, ApiError> {
    let public = public_host(public_url);
    let server = match server.map(str::trim) {
        Some(server) if !server.is_empty() => server.to_owned(),
        _ => {
            let host = if public.contains(':') && !public.starts_with('[') {
                format!("[{public}]")
            } else {
                public.clone()
            };
            format!("{host}:{DEFAULT_AGENT_PORT}")
        }
    };
    if !crate::msi::valid_host_port(&server) {
        return Err(ApiError::BadRequest(
            "server must be host:port (e.g. rmm.example.com:4433)".into(),
        ));
    }
    let server_name = match server_name.map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => {
            let (host, _) = server.rsplit_once(':').expect("checked above");
            let host = host.trim_start_matches('[').trim_end_matches(']');
            if host.parse::<std::net::IpAddr>().is_ok() {
                public
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_owned()
            } else {
                host.to_owned()
            }
        }
    };
    if !crate::msi::valid_host(&server_name) {
        return Err(ApiError::BadRequest(
            "server_name must be a host name or IP address".into(),
        ));
    }
    Ok(enroll::InstallTarget {
        server,
        server_name,
    })
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
    if !req.group_ids.is_empty() {
        // Group membership extends grants: only admins decide it.
        rbac::require_admin(&state.pool, &session.user, "enrollment_link_groups").await?;
        crate::groups::check_exist(&state.pool, &req.group_ids)
            .await
            .map_err(rbac::group_error)?;
    }
    let install = install_target(
        &state.public_url,
        req.server.as_deref(),
        req.server_name.as_deref(),
    )?;
    let minted = enroll::create_link_token(
        &state.pool,
        &session.user.username,
        ttl,
        &req.group_ids,
        Some(&install),
    )
    .await?;
    let base = format!("{}/api/download/{platform}", state.public_url);
    Ok(Json(EnrollmentLinkResponse {
        download_url: format!("{base}?token={}", minted.token),
        msi_url: (platform == MSI_PLATFORM).then(|| format!("{base}/msi?token={}", minted.token)),
        token: minted.token,
        expires_at: minted.expires_at,
        server: install.server,
        server_name: install.server_name,
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

/// The latest Windows agent build as an MSI that installs and enrolls it
/// unattended (see `crate::msi`). Like [`download_agent`], needs a usable
/// enrollment token and does not consume it.
async fn download_msi(
    State(state): State<AppState>,
    Path(platform): Path<String>,
    Query(query): Query<DownloadQuery>,
) -> Result<Response, ApiError> {
    let Some(install) = enroll::usable_install_target(&state.pool, &query.token).await? else {
        return Err(ApiError::Unauthorized(
            "download link is invalid or expired",
        ));
    };
    if platform != MSI_PLATFORM {
        return Err(ApiError::NotFound);
    }
    // Links made before install targets existed get the defaults.
    let install = match install {
        Some(install) => install,
        None => install_target(&state.public_url, None, None)?,
    };
    let manifest = updates::load_manifest(&state.updates_dir, &platform)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    let binary = tokio::fs::read(state.updates_dir.join(&platform).join(updates::BINARY_FILE))
        .await
        .map_err(|e| ApiError::Internal(format!("reading the agent build: {e}")))?;
    let ca_pem = state.server_ca_pem.clone();
    let token = query.token;
    let version = manifest.version.clone();
    let msi = tokio::task::spawn_blocking(move || {
        crate::msi::build(&crate::msi::MsiConfig {
            agent_exe: &binary,
            version: &version,
            ca_pem: &ca_pem,
            server: &install.server,
            server_name: &install.server_name,
            token: &token,
        })
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| ApiError::Internal(format!("building the MSI: {e}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-msi".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"rmm-agent-{}.msi\"",
                    manifest.version
                ),
            ),
        ],
        msi,
    )
        .into_response())
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

/// The update signing key's public half (hex). Agents built without one
/// fetch it over this CA-verified connection and pin it.
async fn update_public_key(State(state): State<AppState>) -> Result<String, ApiError> {
    updates::load_public_key(&state.updates_dir)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::NotFound)
}

/// The CA certificate that signed the server's and the agents' certificates
/// (PEM). The support TUI fetches it for the viewer, and to offer it for
/// trust the first time it meets a server whose certificate this CA signed.
async fn ca_certificate(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/x-pem-file")],
        state.server_ca_pem.clone(),
    )
        .into_response()
}

async fn viewer_manifest(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Result<Json<protocol::update::UpdateManifest>, ApiError> {
    updates::load_viewer_manifest(&state.updates_dir, &platform)
        .map_err(|e| match e {
            updates::UpdateError::InvalidPlatform(_) => ApiError::NotFound,
            other => ApiError::Internal(other.to_string()),
        })?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

async fn viewer_binary(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Result<Response, ApiError> {
    serve_update_file(&state.updates_dir, &platform, updates::VIEWER_BINARY_FILE).await
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
    // Audit a refusal even for a request that is not a WebSocket.
    remote::authorize(
        &state.pool,
        &session.user,
        crate::users::Capability::Shell,
        std::slice::from_ref(&agent_id),
    )
    .await?;
    let upgrade = upgrade.map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let opened = shell::open(&state.pool, hub(&state)?, &session.user, &agent_id, size).await?;
    Ok(upgrade.on_upgrade(move |socket| shell::relay(socket, opened)))
}

#[derive(Deserialize)]
struct ScriptRunRequest {
    #[serde(default)]
    agent_ids: Vec<String>,
    /// Groups whose members are targeted too (at the time of the run).
    #[serde(default)]
    group_ids: Vec<i64>,
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
    let mut targets = req.agent_ids;
    if !req.group_ids.is_empty() {
        let members = crate::groups::members_of(&state.pool, &req.group_ids)
            .await
            .map_err(rbac::group_error)?;
        if members.is_empty() && targets.is_empty() {
            return Err(ApiError::BadRequest("the groups have no agents".into()));
        }
        targets.extend(members);
    }
    let agents = script::normalize_agents(&targets)?;
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
