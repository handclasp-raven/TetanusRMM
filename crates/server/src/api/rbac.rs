//! Administration endpoints for RBAC: users and their roles, access grants,
//! and agent groups (see the table in `super` and `crate::access`).
//!
//! Only admins change anything here. A non-admin's attempt is refused and
//! audited as `permission.denied` with the action it tried. Anyone signed
//! in may read the groups (members limited to agents they can see) and
//! their own grants.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;

use super::{ApiError, AppState, Session};
use crate::access::{self, Capabilities, GrantError, Scope};
use crate::audit::{self, Action, NewEntry};
use crate::groups::{self, Group, GroupError};
use crate::users::{self, Capability, CreateUserError, Role, User, UserAdminError};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}/role", put(set_role))
        .route("/api/users/{id}/password", put(set_password))
        .route("/api/users/{id}/totp", post(reset_totp))
        .route("/api/users/{id}", delete(delete_user))
        .route("/api/grants", get(list_grants).post(create_grant))
        .route("/api/grants/{id}", delete(delete_grant))
        .route("/api/groups", get(list_groups).post(create_group))
        .route(
            "/api/groups/{id}",
            get(get_group).patch(update_group).delete(delete_group),
        )
        .route(
            "/api/groups/{id}/agents",
            put(set_members).post(add_members),
        )
        .route("/api/groups/{id}/agents/{agent_id}", delete(remove_member))
}

/// Refuse (and audit) unless `user` is an admin. `action` names what was
/// attempted, for the audit log.
pub async fn require_admin(pool: &PgPool, user: &User, action: &str) -> Result<(), ApiError> {
    if user.role == Role::Admin {
        return Ok(());
    }
    crate::metrics::get()
        .permission_denied
        .get_or_create(&crate::metrics::DeniedLabels {
            capability: "admin",
        })
        .inc();
    audit::append_now(
        pool,
        NewEntry::new(&user.username, Action::PermissionDenied)
            .detail(json!({ "action": action, "role": user.role })),
    )
    .await?;
    Err(ApiError::Forbidden)
}

pub fn group_error(e: GroupError) -> ApiError {
    match e {
        GroupError::NotFound => ApiError::NotFound,
        GroupError::Duplicate => ApiError::Status(StatusCode::CONFLICT, e.to_string()),
        GroupError::Db(e) => e.into(),
        other => ApiError::BadRequest(other.to_string()),
    }
}

fn grant_error(e: GrantError) -> ApiError {
    match e {
        GrantError::NotFound => ApiError::NotFound,
        GrantError::Db(e) => e.into(),
        other => ApiError::BadRequest(other.to_string()),
    }
}

fn user_admin_error(e: UserAdminError) -> ApiError {
    match e {
        UserAdminError::NotFound => ApiError::NotFound,
        UserAdminError::LastAdmin | UserAdminError::SelfDelete => {
            ApiError::Status(StatusCode::CONFLICT, e.to_string())
        }
        UserAdminError::WeakPassword => ApiError::BadRequest(e.to_string()),
        UserAdminError::Db(e) => e.into(),
        other => ApiError::Internal(other.to_string()),
    }
}

// --- Users ---------------------------------------------------------------

async fn list_users(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<Vec<User>>, ApiError> {
    require_admin(&state.pool, &session.user, "user.list").await?;
    Ok(Json(users::list_users(&state.pool).await?))
}

#[derive(Deserialize)]
struct CreateUserRequest {
    username: String,
    password: String,
    role: Role,
}

#[derive(Serialize)]
struct CreatedUserResponse {
    user: User,
    /// Hand these to the user for their authenticator app; they are not
    /// shown again.
    totp_secret: String,
    otpauth_url: String,
}

async fn create_user(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<CreatedUserResponse>), ApiError> {
    require_admin(&state.pool, &session.user, "user.create").await?;
    let created = users::create_user(
        &state.pool,
        &session.user.username,
        &req.username,
        &req.password,
        req.role,
    )
    .await
    .map_err(|e| match e {
        CreateUserError::Duplicate => ApiError::Status(StatusCode::CONFLICT, e.to_string()),
        CreateUserError::InvalidUsername | CreateUserError::WeakPassword => {
            ApiError::BadRequest(e.to_string())
        }
        other => ApiError::Internal(other.to_string()),
    })?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedUserResponse {
            user: created.user,
            totp_secret: created.totp_secret,
            otpauth_url: created.otpauth_url,
        }),
    ))
}

#[derive(Deserialize)]
struct RoleRequest {
    role: Role,
}

async fn set_role(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Json(req): Json<RoleRequest>,
) -> Result<Json<User>, ApiError> {
    require_admin(&state.pool, &session.user, "user.role_change").await?;
    users::set_role(&state.pool, &session.user.username, id, req.role)
        .await
        .map(Json)
        .map_err(user_admin_error)
}

#[derive(Deserialize)]
struct PasswordRequest {
    password: String,
}

/// The session to keep when an admin changes their own credentials;
/// anyone else's sessions all end.
fn own_session(session: &Session, user_id: i64) -> Option<&str> {
    (session.user.id == user_id).then_some(session.token.as_str())
}

async fn set_password(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Json(req): Json<PasswordRequest>,
) -> Result<Json<User>, ApiError> {
    require_admin(&state.pool, &session.user, "user.password_change").await?;
    users::set_password(
        &state.pool,
        &session.user.username,
        id,
        &req.password,
        own_session(&session, id),
    )
    .await
    .map(Json)
    .map_err(user_admin_error)
}

async fn reset_totp(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<Json<CreatedUserResponse>, ApiError> {
    require_admin(&state.pool, &session.user, "user.totp_reset").await?;
    let reset = users::reset_totp(
        &state.pool,
        &session.user.username,
        id,
        own_session(&session, id),
    )
    .await
    .map_err(user_admin_error)?;
    Ok(Json(CreatedUserResponse {
        user: reset.user,
        totp_secret: reset.totp_secret,
        otpauth_url: reset.otpauth_url,
    }))
}

async fn delete_user(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state.pool, &session.user, "user.delete").await?;
    users::delete_user(&state.pool, &session.user, id)
        .await
        .map_err(user_admin_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Grants --------------------------------------------------------------

#[derive(Deserialize)]
struct GrantQuery {
    user_id: Option<i64>,
}

/// All grants (admins), or one user's. Users may always list their own.
async fn list_grants(
    State(state): State<AppState>,
    session: Session,
    Query(query): Query<GrantQuery>,
) -> Result<Json<Vec<access::Grant>>, ApiError> {
    if query.user_id != Some(session.user.id) {
        require_admin(&state.pool, &session.user, "grant.list").await?;
    }
    Ok(Json(access::list_grants(&state.pool, query.user_id).await?))
}

#[derive(Deserialize)]
struct GrantRequest {
    user_id: i64,
    /// Exactly one of `agent_id`, `group_id` and `all_agents: true`.
    agent_id: Option<String>,
    group_id: Option<i64>,
    #[serde(default)]
    all_agents: bool,
    /// Defaults to every capability.
    capabilities: Option<Vec<String>>,
}

impl GrantRequest {
    fn scope(&self) -> Result<Scope, ApiError> {
        match (&self.agent_id, self.group_id, self.all_agents) {
            (Some(agent), None, false) => Ok(Scope::Agent(agent.clone())),
            (None, Some(group), false) => Ok(Scope::Group(group)),
            (None, None, true) => Ok(Scope::AllAgents),
            _ => Err(ApiError::BadRequest(
                "give exactly one of agent_id, group_id and all_agents".into(),
            )),
        }
    }

    fn capabilities(&self) -> Result<Capabilities, ApiError> {
        match &self.capabilities {
            None => Ok(access::all_capabilities()),
            Some(names) => names
                .iter()
                .map(|n| {
                    Capability::parse(n).ok_or_else(|| {
                        ApiError::BadRequest(format!(
                            "unknown capability {n:?} (expected desktop, shell, script or file_transfer)"
                        ))
                    })
                })
                .collect(),
        }
    }
}

async fn create_grant(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<GrantRequest>,
) -> Result<(StatusCode, Json<access::Grant>), ApiError> {
    require_admin(&state.pool, &session.user, "grant.create").await?;
    let grant = access::create_grant(
        &state.pool,
        &session.user.username,
        req.user_id,
        req.scope()?,
        &req.capabilities()?,
    )
    .await
    .map_err(grant_error)?;
    Ok((StatusCode::CREATED, Json(grant)))
}

async fn delete_grant(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state.pool, &session.user, "grant.delete").await?;
    access::delete_grant(&state.pool, &session.user.username, id)
        .await
        .map_err(grant_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Groups --------------------------------------------------------------

/// Hide members the user cannot see.
async fn visible_members(
    pool: &PgPool,
    user: &User,
    mut groups: Vec<Group>,
) -> Result<Vec<Group>, ApiError> {
    let visibility = access::visibility(pool, user).await?;
    for group in &mut groups {
        group.agent_ids.retain(|a| visibility.sees(a));
    }
    Ok(groups)
}

async fn list_groups(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<Vec<Group>>, ApiError> {
    let groups = groups::list(&state.pool).await?;
    Ok(Json(
        visible_members(&state.pool, &session.user, groups).await?,
    ))
}

async fn get_group(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<Json<Group>, ApiError> {
    let group = groups::get(&state.pool, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut groups = visible_members(&state.pool, &session.user, vec![group]).await?;
    Ok(Json(groups.remove(0)))
}

#[derive(Deserialize)]
struct CreateGroupRequest {
    name: String,
    #[serde(default)]
    description: String,
    /// Initial members.
    #[serde(default)]
    agent_ids: Vec<String>,
}

async fn create_group(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<Group>), ApiError> {
    require_admin(&state.pool, &session.user, "group.create").await?;
    let actor = &session.user.username;
    let mut group = groups::create(&state.pool, actor, &req.name, &req.description)
        .await
        .map_err(group_error)?;
    if !req.agent_ids.is_empty() {
        group = groups::change_members(&state.pool, actor, group.id, &req.agent_ids, &[])
            .await
            .map_err(group_error)?;
    }
    Ok((StatusCode::CREATED, Json(group)))
}

#[derive(Deserialize)]
struct UpdateGroupRequest {
    name: Option<String>,
    description: Option<String>,
}

async fn update_group(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Json(req): Json<UpdateGroupRequest>,
) -> Result<Json<Group>, ApiError> {
    require_admin(&state.pool, &session.user, "group.update").await?;
    groups::update(
        &state.pool,
        &session.user.username,
        id,
        req.name.as_deref(),
        req.description.as_deref(),
    )
    .await
    .map(Json)
    .map_err(group_error)
}

async fn delete_group(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    require_admin(&state.pool, &session.user, "group.delete").await?;
    groups::delete(&state.pool, &session.user.username, id)
        .await
        .map_err(group_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MembersRequest {
    agent_ids: Vec<String>,
}

/// Make the members exactly `agent_ids`.
async fn set_members(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Json(req): Json<MembersRequest>,
) -> Result<Json<Group>, ApiError> {
    require_admin(&state.pool, &session.user, "group.members").await?;
    groups::set_members(&state.pool, &session.user.username, id, &req.agent_ids)
        .await
        .map(Json)
        .map_err(group_error)
}

/// Add `agent_ids` to the members.
async fn add_members(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
    Json(req): Json<MembersRequest>,
) -> Result<Json<Group>, ApiError> {
    require_admin(&state.pool, &session.user, "group.members").await?;
    groups::change_members(&state.pool, &session.user.username, id, &req.agent_ids, &[])
        .await
        .map(Json)
        .map_err(group_error)
}

async fn remove_member(
    State(state): State<AppState>,
    session: Session,
    Path((id, agent_id)): Path<(i64, String)>,
) -> Result<Json<Group>, ApiError> {
    require_admin(&state.pool, &session.user, "group.members").await?;
    groups::change_members(&state.pool, &session.user.username, id, &[], &[agent_id])
        .await
        .map(Json)
        .map_err(group_error)
}
