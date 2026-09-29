//! Two-step login (password, then TOTP) and server-side sessions.
//!
//! 1. [`login_password`] checks the password and returns a short-lived
//!    challenge token (a `pending_totp` session row).
//! 2. [`login_totp`] checks the TOTP code against that challenge, deletes it,
//!    and returns a new session token (an `active` session row).
//!
//! Tokens are 256 random bits, hex-encoded. Only their SHA-256 is stored, so a
//! database leak does not hand out live sessions.

pub mod password;
pub mod totp;

use std::time::Duration;

use chrono::{DateTime, Utc};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::json;
use sqlx::PgPool;

use crate::audit::{self, Action, NewEntry};
use crate::users::{Role, User};

/// How long a password-verified login waits for its TOTP code.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);

/// Wrong TOTP codes allowed per challenge before it is discarded and the user
/// must start again with their password.
pub const MAX_TOTP_ATTEMPTS: i32 = 5;

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub session_ttl: Duration,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            session_ttl: Duration::from_secs(12 * 60 * 60),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// Unknown user or wrong password. Deliberately indistinguishable.
    #[error("invalid username or password")]
    InvalidCredentials,
    /// Challenge token unknown, expired, or used up.
    #[error("login challenge is invalid or expired")]
    InvalidChallenge,
    /// Wrong, expired, or already-used TOTP code.
    #[error("invalid TOTP code")]
    InvalidTotp,
    #[error(transparent)]
    Totp(#[from] totp::TotpError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Result of a correct password: present `challenge_token` with a TOTP code.
#[derive(Debug)]
pub struct Challenge {
    pub challenge_token: String,
    pub expires_in: Duration,
}

/// Result of a correct TOTP code.
#[derive(Debug)]
pub struct SessionGrant {
    pub token: String,
    pub user: User,
    pub expires_at: DateTime<Utc>,
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG available");
    hex::encode(bytes)
}

pub fn token_hash(token: &str) -> Vec<u8> {
    digest(&SHA256, token.as_bytes()).as_ref().to_vec()
}

#[derive(sqlx::FromRow)]
struct Credentials {
    id: i64,
    password_hash: String,
}

/// Step 1: verify username and password.
pub async fn login_password(
    pool: &PgPool,
    username: &str,
    password: &str,
) -> Result<Challenge, LoginError> {
    let creds: Option<Credentials> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = $1")
            .bind(username)
            .fetch_optional(pool)
            .await?;
    let user_id = creds.as_ref().map(|c| c.id);
    let ok = password::verify(password.to_owned(), creds.map(|c| c.password_hash)).await;

    let Some(user_id) = user_id.filter(|_| ok) else {
        audit::append_now(
            pool,
            NewEntry::new(username, Action::LoginBadPassword).detail(json!({
                "reason": if user_id.is_some() { "wrong_password" } else { "unknown_user" }
            })),
        )
        .await?;
        return Err(LoginError::InvalidCredentials);
    };

    let token = new_token();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, stage, expires_at)
         VALUES ($1, $2, 'pending_totp', now() + make_interval(secs => $3))",
    )
    .bind(token_hash(&token))
    .bind(user_id)
    .bind(CHALLENGE_TTL.as_secs_f64())
    .execute(pool)
    .await?;

    Ok(Challenge {
        challenge_token: token,
        expires_in: CHALLENGE_TTL,
    })
}

#[derive(sqlx::FromRow)]
struct PendingLogin {
    user_id: i64,
    username: String,
    role: Role,
    totp_secret: String,
}

/// Step 2: verify the TOTP code for a challenge and open a session.
pub async fn login_totp(
    pool: &PgPool,
    settings: &AuthSettings,
    challenge_token: &str,
    code: &str,
) -> Result<SessionGrant, LoginError> {
    let challenge_hash = token_hash(challenge_token);
    let mut tx = pool.begin().await?;

    // Lock the challenge row so concurrent attempts are counted one at a time.
    let pending: Option<PendingLogin> = sqlx::query_as(
        "SELECT s.user_id, u.username, u.role, u.totp_secret
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.stage = 'pending_totp' AND s.expires_at > now()
         FOR UPDATE OF s",
    )
    .bind(&challenge_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(pending) = pending else {
        return Err(LoginError::InvalidChallenge);
    };

    let step = totp::check(&pending.totp_secret, code, totp::unix_now())?;
    // Accept each step at most once per user (RFC 6238 section 5.2). The
    // conditional UPDATE makes this atomic across concurrent logins.
    let accepted = match step {
        Some(step) => {
            sqlx::query(
                "UPDATE users SET totp_last_step = $2
                 WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)",
            )
            .bind(pending.user_id)
            .bind(step as i64)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1
        }
        None => false,
    };

    if !accepted {
        let attempts: i32 = sqlx::query_scalar(
            "UPDATE sessions SET failed_attempts = failed_attempts + 1
             WHERE token_hash = $1 RETURNING failed_attempts",
        )
        .bind(&challenge_hash)
        .fetch_one(&mut *tx)
        .await?;
        if attempts >= MAX_TOTP_ATTEMPTS {
            sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
                .bind(&challenge_hash)
                .execute(&mut *tx)
                .await?;
        }
        audit::append(
            &mut tx,
            NewEntry::new(&pending.username, Action::LoginBadTotp).detail(json!({
                "reason": if step.is_some() { "code_reused" } else { "wrong_code" },
                "attempts": attempts,
                "challenge_discarded": attempts >= MAX_TOTP_ATTEMPTS,
            })),
        )
        .await?;
        tx.commit().await?;
        return Err(LoginError::InvalidTotp);
    }

    // Replace the challenge with a new session token, so a leaked challenge
    // token can never become a session.
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&challenge_hash)
        .execute(&mut *tx)
        .await?;
    // Opportunistic cleanup of expired rows.
    sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(&mut *tx)
        .await?;

    let token = new_token();
    let expires_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO sessions (token_hash, user_id, stage, expires_at)
         VALUES ($1, $2, 'active', now() + make_interval(secs => $3))
         RETURNING expires_at",
    )
    .bind(token_hash(&token))
    .bind(pending.user_id)
    .bind(settings.session_ttl.as_secs_f64())
    .fetch_one(&mut *tx)
    .await?;

    audit::append(
        &mut tx,
        NewEntry::new(&pending.username, Action::LoginSuccess)
            .detail(json!({ "user_id": pending.user_id, "role": pending.role })),
    )
    .await?;
    tx.commit().await?;

    Ok(SessionGrant {
        token,
        user: User {
            id: pending.user_id,
            username: pending.username,
            role: pending.role,
        },
        expires_at,
    })
}

/// The user behind an active, unexpired session token.
pub async fn authenticate(pool: &PgPool, token: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as(
        "SELECT u.id, u.username, u.role
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.stage = 'active' AND s.expires_at > now()",
    )
    .bind(token_hash(token))
    .fetch_optional(pool)
    .await
}

/// End the session for `token`. Audited.
pub async fn logout(pool: &PgPool, user: &User, token: &str) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1 AND stage = 'active'")
        .bind(token_hash(token))
        .execute(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(&user.username, Action::Logout).detail(json!({ "user_id": user.id })),
    )
    .await?;
    tx.commit().await
}
