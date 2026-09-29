//! User accounts and roles.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;

use crate::audit::{self, Action, NewEntry};
use crate::auth::{password, totp};

/// Shortest password `create_user` accepts.
pub const MIN_PASSWORD_LEN: usize = 12;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, clap::ValueEnum,
)]
#[sqlx(type_name = "user_role", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    SupportEngineer,
    /// Read-only: may view everything, change nothing.
    Auditor,
}

impl Role {
    /// Whether this role may change device policies.
    pub fn can_edit_policies(self) -> bool {
        matches!(self, Role::Admin)
    }

    /// Whether this role may watch agents' screens.
    pub fn can_view_desktop(self) -> bool {
        matches!(self, Role::Admin | Role::SupportEngineer)
    }

    /// Whether this role may read the audit log.
    pub fn can_read_audit(self) -> bool {
        matches!(self, Role::Admin | Role::Auditor)
    }

    /// Whether this role may create agent download links.
    pub fn can_create_enrollment_links(self) -> bool {
        matches!(self, Role::Admin | Role::SupportEngineer)
    }

    /// Whether this role may ever use `capability` on an agent. Admins
    /// may anywhere; support engineers only where a grant says so (see
    /// `crate::access`); auditors never.
    pub fn can(self, capability: Capability) -> bool {
        match capability {
            Capability::Desktop
            | Capability::Shell
            | Capability::Script
            | Capability::FileTransfer => matches!(self, Role::Admin | Role::SupportEngineer),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::SupportEngineer => "support_engineer",
            Role::Auditor => "auditor",
        }
    }
}

/// Things a user can do to an agent, each granted separately (see
/// `crate::access`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Remote desktop: view, input and clipboard.
    Desktop,
    /// Interactive remote shell.
    Shell,
    /// Running scripts and commands.
    Script,
    /// Uploading and downloading files.
    FileTransfer,
}

impl Capability {
    pub const ALL: [Capability; 4] = [
        Capability::Desktop,
        Capability::Shell,
        Capability::Script,
        Capability::FileTransfer,
    ];

    /// The name stored in grants and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Desktop => "desktop",
            Capability::Shell => "shell",
            Capability::Script => "script",
            Capability::FileTransfer => "file_transfer",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
}

/// A freshly created user and the TOTP secret to load into an authenticator app.
#[derive(Debug)]
pub struct CreatedUser {
    pub user: User,
    pub totp_secret: String,
    pub otpauth_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateUserError {
    #[error("username must be 1-64 characters of letters, digits, '.', '_', '-' or '@'")]
    InvalidUsername,
    #[error("password must be at least {MIN_PASSWORD_LEN} characters")]
    WeakPassword,
    #[error("username already exists")]
    Duplicate,
    #[error(transparent)]
    Password(#[from] password::HashError),
    #[error(transparent)]
    Totp(#[from] totp::TotpError),
    #[error(transparent)]
    Db(sqlx::Error),
}

fn valid_username(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// Create a user with a fresh TOTP secret. Audited as `user.create` by `actor`.
pub async fn create_user(
    pool: &PgPool,
    actor: &str,
    username: &str,
    password: &str,
    role: Role,
) -> Result<CreatedUser, CreateUserError> {
    if !valid_username(username) {
        return Err(CreateUserError::InvalidUsername);
    }
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(CreateUserError::WeakPassword);
    }
    let password_hash = password::hash(password.to_owned()).await?;
    let secret = totp::generate_secret();
    let otpauth_url = totp::otpauth_url(&secret, username)?;

    let mut tx = pool.begin().await.map_err(CreateUserError::Db)?;
    let user: User = sqlx::query_as(
        "INSERT INTO users (username, password_hash, totp_secret, role)
         VALUES ($1, $2, $3, $4)
         RETURNING id, username, role",
    )
    .bind(username)
    .bind(&password_hash)
    .bind(&secret)
    .bind(role)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => CreateUserError::Duplicate,
        _ => CreateUserError::Db(e),
    })?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::UserCreate)
            .target(username)
            .detail(json!({ "role": role, "user_id": user.id })),
    )
    .await
    .map_err(CreateUserError::Db)?;
    tx.commit().await.map_err(CreateUserError::Db)?;

    Ok(CreatedUser {
        user,
        totp_secret: secret,
        otpauth_url,
    })
}

/// All users, by username.
pub async fn list_users(pool: &PgPool) -> sqlx::Result<Vec<User>> {
    sqlx::query_as("SELECT id, username, role FROM users ORDER BY username")
        .fetch_all(pool)
        .await
}

#[derive(Debug, thiserror::Error)]
pub enum UserAdminError {
    #[error("no such user")]
    NotFound,
    #[error("that would leave no admin")]
    LastAdmin,
    #[error("you cannot delete your own account")]
    SelfDelete,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Lock `user_id` and the admin set; errors if removing admin rights from
/// `user_id` would leave none.
async fn lock_for_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: i64,
    losing_admin: impl Fn(Role) -> bool,
) -> Result<User, UserAdminError> {
    // Lock every admin row, so two admins cannot demote each other at once.
    let admins: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE role = 'admin' ORDER BY id FOR UPDATE")
            .fetch_all(&mut **tx)
            .await?;
    let user: User =
        sqlx::query_as("SELECT id, username, role FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(UserAdminError::NotFound)?;
    if user.role == Role::Admin && losing_admin(user.role) && admins.len() <= 1 {
        return Err(UserAdminError::LastAdmin);
    }
    Ok(user)
}

/// Change a user's role. Takes effect on their next request (roles are
/// read per request). Audited as `user.role_change` by `actor`. Grants are
/// kept, but only mean something while the user is a support engineer.
pub async fn set_role(
    pool: &PgPool,
    actor: &str,
    user_id: i64,
    role: Role,
) -> Result<User, UserAdminError> {
    let mut tx = pool.begin().await?;
    let before = lock_for_change(&mut tx, user_id, |_| role != Role::Admin).await?;
    if before.role == role {
        return Ok(before);
    }
    sqlx::query("UPDATE users SET role = $2 WHERE id = $1")
        .bind(user_id)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::UserRoleChange)
            .target(&before.username)
            .detail(json!({ "user_id": user_id, "before": before.role, "after": role })),
    )
    .await?;
    tx.commit().await?;
    Ok(User { role, ..before })
}

/// Delete a user: their sessions and grants go too; the audit log keeps
/// their name. Audited as `user.delete` by `actor`.
pub async fn delete_user(pool: &PgPool, actor: &User, user_id: i64) -> Result<(), UserAdminError> {
    if actor.id == user_id {
        return Err(UserAdminError::SelfDelete);
    }
    let mut tx = pool.begin().await?;
    let user = lock_for_change(&mut tx, user_id, |_| true).await?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(&actor.username, Action::UserDelete)
            .target(&user.username)
            .detail(json!({ "user_id": user_id, "role": user.role })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_rules() {
        assert!(valid_username("alice"));
        assert!(valid_username("jane.doe@example.com"));
        assert!(!valid_username(""));
        assert!(!valid_username("has space"));
        assert!(!valid_username(&"a".repeat(65)));
    }

    #[test]
    fn only_admins_edit_policies() {
        assert!(Role::Admin.can_edit_policies());
        assert!(!Role::SupportEngineer.can_edit_policies());
        assert!(!Role::Auditor.can_edit_policies());
    }

    #[test]
    fn auditors_cannot_view_desktops() {
        assert!(Role::Admin.can_view_desktop());
        assert!(Role::SupportEngineer.can_view_desktop());
        assert!(!Role::Auditor.can_view_desktop());
    }

    #[test]
    fn capability_names_round_trip_and_match_serde() {
        for capability in Capability::ALL {
            assert_eq!(Capability::parse(capability.as_str()), Some(capability));
            assert_eq!(
                serde_json::to_value(capability).unwrap(),
                capability.as_str()
            );
        }
        assert_eq!(Capability::parse("root"), None);
    }

    #[test]
    fn only_admins_and_engineers_run_remote_operations() {
        for capability in Capability::ALL {
            assert!(Role::Admin.can(capability));
            assert!(Role::SupportEngineer.can(capability));
            assert!(!Role::Auditor.can(capability), "{capability:?}");
        }
    }

    #[test]
    fn admins_and_auditors_read_the_audit_log() {
        assert!(Role::Admin.can_read_audit());
        assert!(Role::Auditor.can_read_audit());
        assert!(!Role::SupportEngineer.can_read_audit());
    }

    #[test]
    fn admins_and_engineers_create_download_links() {
        assert!(Role::Admin.can_create_enrollment_links());
        assert!(Role::SupportEngineer.can_create_enrollment_links());
        assert!(!Role::Auditor.can_create_enrollment_links());
    }
}
