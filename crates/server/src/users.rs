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

    /// Whether this role may use a remote operation on agents.
    pub fn can(self, capability: Capability) -> bool {
        match capability {
            Capability::Shell | Capability::Script | Capability::FileTransfer => {
                matches!(self, Role::Admin | Role::SupportEngineer)
            }
        }
    }
}

/// Remote operations on an agent that need a role check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Interactive remote shell.
    Shell,
    /// Running scripts and commands.
    Script,
    /// Uploading and downloading files.
    FileTransfer,
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
    fn only_admins_and_engineers_run_remote_operations() {
        for capability in [
            Capability::Shell,
            Capability::Script,
            Capability::FileTransfer,
        ] {
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
