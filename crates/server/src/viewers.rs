//! Viewer sessions: short-lived, single-use tokens that let a viewer attach
//! to one agent's screen through the relay.
//!
//! A user asks the HTTPS API for a token for a specific agent ([`create`]).
//! The viewer (launched by the TUI) presents it in `ViewerHello` on its QUIC
//! connection; [`connect`] consumes it. Tokens live [`TOKEN_TTL`], work once,
//! and only their SHA-256 is stored. Create, connect and disconnect are all
//! audited, as are the consent outcome (`session.start`), a Ctrl+F12
//! termination (`session.user_terminated`), and each move between the relay
//! and a direct path (`session.path`). Sessions are end-to-end encrypted,
//! so none of these can (or do) record anything of their content.
//!
//! Access (the `desktop` capability, see `crate::access`) is checked when
//! the token is minted and again when it is used, so a grant revoked in
//! between still stops the session. Refusals are audited as
//! `permission.denied`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use protocol::consent::{ConsentMode, Outcome};
use protocol::e2e::Path;
use serde_json::json;
use sqlx::PgPool;

use crate::access;
use crate::audit::{self, Action, NewEntry};
use crate::auth::{new_token, token_hash};
use crate::users::{Capability, Role, User};

/// How long a viewer-session token stays valid if unused.
pub const TOKEN_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ViewerError {
    #[error("not allowed to view this agent")]
    Forbidden,
    #[error("no such agent")]
    UnknownAgent,
    #[error("viewer token is invalid, expired or already used")]
    InvalidToken,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug)]
pub struct NewViewerSession {
    pub id: i64,
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Mint a viewer-session token for `agent_id`. Audited as `viewer.session_create`.
pub async fn create(
    pool: &PgPool,
    user: &User,
    agent_id: &str,
) -> Result<NewViewerSession, ViewerError> {
    let agents = [agent_id.to_owned()];
    if access::check(pool, user, Capability::Desktop, &agents)
        .await?
        .is_err()
    {
        return Err(ViewerError::Forbidden);
    }
    let token = new_token();
    let mut tx = pool.begin().await?;
    let row: Option<(i64, DateTime<Utc>)> = sqlx::query_as(
        "INSERT INTO viewer_sessions (token_hash, user_id, agent_id, expires_at)
         SELECT $1, $2, id, now() + make_interval(secs => $4)
         FROM agents WHERE id = $3
         RETURNING id, expires_at",
    )
    .bind(token_hash(&token))
    .bind(user.id)
    .bind(agent_id)
    .bind(TOKEN_TTL.as_secs_f64())
    .fetch_optional(&mut *tx)
    .await?;
    let (id, expires_at) = row.ok_or(ViewerError::UnknownAgent)?;
    audit::append(
        &mut tx,
        NewEntry::new(&user.username, Action::ViewerSessionCreate)
            .target(agent_id)
            .detail(json!({ "viewer_session_id": id })),
    )
    .await?;
    tx.commit().await?;
    Ok(NewViewerSession {
        id,
        token,
        expires_at,
    })
}

#[derive(sqlx::FromRow)]
struct ViewerGrantRow {
    session_id: i64,
    user_id: i64,
    username: String,
    agent_id: String,
    role: Role,
}

/// A consumed token: who is watching which agent.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ViewerGrant {
    pub session_id: i64,
    pub user_id: i64,
    pub username: String,
    pub agent_id: String,
}

/// Consume a token on connect. Single-use and time-limited. Audited as
/// `viewer.connect`. The user's access is re-checked, in case their role
/// or grants changed since the token was minted; a refusal still uses up
/// the token and is audited as `permission.denied`.
pub async fn connect(pool: &PgPool, token: &str) -> Result<ViewerGrant, ViewerError> {
    let mut tx = pool.begin().await?;
    let row: Option<(ViewerGrant, Role)> = sqlx::query_as(
        "UPDATE viewer_sessions v SET connected_at = now()
         FROM users u
         WHERE v.token_hash = $1 AND v.connected_at IS NULL AND v.expires_at > now()
           AND u.id = v.user_id
         RETURNING v.id AS session_id, v.user_id, u.username, v.agent_id, u.role",
    )
    .bind(token_hash(token))
    .fetch_optional(&mut *tx)
    .await?
    .map(|row: ViewerGrantRow| {
        (
            ViewerGrant {
                session_id: row.session_id,
                user_id: row.user_id,
                username: row.username,
                agent_id: row.agent_id,
            },
            row.role,
        )
    });
    let (grant, role) = row.ok_or(ViewerError::InvalidToken)?;
    let user = User {
        id: grant.user_id,
        username: grant.username.clone(),
        role,
    };
    let allowed = access::agent_capabilities(&mut *tx, &user, &grant.agent_id)
        .await?
        .contains(&Capability::Desktop);
    if !allowed {
        tx.commit().await?;
        access::audit_denied(pool, &user, Capability::Desktop, &[grant.agent_id]).await?;
        return Err(ViewerError::Forbidden);
    }
    audit::append(
        &mut tx,
        NewEntry::new(&grant.username, Action::ViewerConnect)
            .target(&grant.agent_id)
            .detail(json!({ "viewer_session_id": grant.session_id })),
    )
    .await?;
    tx.commit().await?;
    Ok(grant)
}

/// Record the end of a viewer session. Audited as `viewer.disconnect`,
/// with the frames the relay sent it and the path it was on at the end.
pub async fn end(
    pool: &PgPool,
    grant: &ViewerGrant,
    frames_sent: u64,
    path: Path,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    let seconds: Option<f64> = sqlx::query_scalar(
        "UPDATE viewer_sessions SET ended_at = now() WHERE id = $1
         RETURNING EXTRACT(EPOCH FROM (ended_at - connected_at))::float8",
    )
    .bind(grant.session_id)
    .fetch_optional(&mut *tx)
    .await?;
    audit::append(
        &mut tx,
        NewEntry::new(&grant.username, Action::ViewerDisconnect)
            .target(&grant.agent_id)
            .detail(json!({
                "viewer_session_id": grant.session_id,
                "duration_secs": seconds,
                "frames_sent": frames_sent,
                "path": path,
            })),
    )
    .await?;
    tx.commit().await
}

/// Record how consent for a viewer session was decided. Audited as
/// `session.start` with the mode in effect and the outcome, whether or not
/// the session was allowed to start.
pub async fn record_consent(
    pool: &PgPool,
    grant: &ViewerGrant,
    mode: ConsentMode,
    outcome: Outcome,
) -> sqlx::Result<()> {
    audit::append_now(
        pool,
        NewEntry::new(&grant.username, Action::SessionStart)
            .target(&grant.agent_id)
            .detail(json!({
                "viewer_session_id": grant.session_id,
                "mode": mode,
                "outcome": outcome,
                "started": outcome.allows_session(),
            })),
    )
    .await
    .map(drop)
}

/// Record that the session moved to `path` (or that a direct attempt
/// failed). Audited as `session.path`.
pub async fn record_path(pool: &PgPool, grant: &ViewerGrant, path: Path) -> sqlx::Result<()> {
    audit::append_now(
        pool,
        NewEntry::new(&grant.username, Action::SessionPath)
            .target(&grant.agent_id)
            .detail(json!({
                "viewer_session_id": grant.session_id,
                "path": path,
            })),
    )
    .await
    .map(drop)
}

/// Record that the user ended this session with Ctrl+F12. Audited as
/// `session.user_terminated` (outcome `user_terminated_session`).
pub async fn record_user_terminated(pool: &PgPool, grant: &ViewerGrant) -> sqlx::Result<()> {
    audit::append_now(
        pool,
        NewEntry::new(&grant.username, Action::SessionUserTerminated)
            .target(&grant.agent_id)
            .detail(json!({
                "viewer_session_id": grant.session_id,
                "outcome": Outcome::UserTerminatedSession,
            })),
    )
    .await
    .map(drop)
}
