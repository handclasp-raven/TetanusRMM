//! Quick assist: one-time support sessions on machines with no agent
//! installed (see `protocol::assist`).
//!
//! 1. A technician asks for a code ([`create`]): six digits, valid for
//!    [`CODE_TTL`], usable once. Only its SHA-256 is stored, and only
//!    while it can still be used.
//! 2. The user runs the quick assist client and types the code. The client
//!    connects without a certificate and sends `AssistEnroll`
//!    (`crate::quic`); [`redeem`] trades the code for a certificate for a
//!    throwaway agent (`qa-…`) valid for [`CERT_VALIDITY`].
//! 3. The client reconnects as that agent. From here on it is an agent like
//!    any other, except that:
//!    - only the technician who made the code (and admins) can reach it,
//!      and only for remote desktop and file transfer
//!      (`crate::access`);
//!    - its consent policy is `require`: the user is asked, by name, before
//!      the technician sees anything;
//!    - it is deleted once it has been gone for [`GONE_AFTER`] ([`prune`]).
//!
//! Six digits are few, so wrong codes are limited ([`Limiter`]). What a
//! correct guess buys is small all the same: the guesser's own machine is
//! offered to the technician, who was expecting someone else.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;

use crate::audit::{self, Action, NewEntry};
use crate::auth::token_hash;
use crate::enroll::{AgentCa, CaError, Enrollment};
use crate::relay::Hub;
use crate::users::{Role, User};

/// How long a code can be redeemed.
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// Validity of a quick assist client's certificate: longer than any
/// session, short enough that a copied one is soon worthless (and it stops
/// working when the agent is deleted, whatever its dates say).
pub const CERT_VALIDITY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a quick assist agent may be disconnected before it is deleted.
/// Long enough to ride out a network change.
pub const GONE_AFTER: Duration = Duration::from_secs(2 * 60);

/// How long the user gets to answer the consent prompt.
const CONSENT_TIMEOUT_SECS: i32 = 60;

/// Sessions are forgotten this long after they were made.
const KEEP_SESSIONS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum AssistError {
    /// Unknown, expired or already-used code. Deliberately indistinguishable.
    #[error("quick assist code is invalid, expired or already used")]
    InvalidCode,
    #[error("no unused code could be found; try again")]
    NoFreeCode,
    #[error(transparent)]
    Ca(#[from] CaError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A freshly made code. `code` is shown once and never stored.
#[derive(Debug)]
pub struct NewCode {
    pub id: i64,
    pub code: String,
    pub expires_at: DateTime<Utc>,
}

/// Six random digits, each code equally likely.
fn random_code() -> String {
    let rng = SystemRandom::new();
    loop {
        let mut bytes = [0u8; 4];
        rng.fill(&mut bytes).expect("system RNG available");
        let n = u32::from_be_bytes(bytes);
        // Reject the top of the range, which would favour low codes.
        if n < u32::MAX - u32::MAX % 1_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

fn new_agent_id() -> String {
    let mut bytes = [0u8; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG available");
    format!("qa-{}", hex::encode(bytes))
}

/// Make a code for `user`. Audited as `assist.create`.
pub async fn create(pool: &PgPool, user: &User) -> Result<NewCode, AssistError> {
    let mut tx = pool.begin().await?;
    // Codes that ran out are free to be handed out again.
    sqlx::query(
        "UPDATE assist_sessions SET code_hash = NULL
         WHERE code_hash IS NOT NULL AND expires_at <= now()",
    )
    .execute(&mut *tx)
    .await?;
    for _ in 0..16 {
        let code = random_code();
        let row: Option<(i64, DateTime<Utc>)> = sqlx::query_as(
            "INSERT INTO assist_sessions (code_hash, user_id, expires_at)
             VALUES ($1, $2, now() + make_interval(secs => $3))
             ON CONFLICT (code_hash) DO NOTHING
             RETURNING id, expires_at",
        )
        .bind(token_hash(&code))
        .bind(user.id)
        .bind(CODE_TTL.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, expires_at)) = row else {
            continue;
        };
        audit::append(
            &mut tx,
            NewEntry::new(&user.username, Action::AssistCreate)
                .detail(json!({ "assist_session_id": id, "expires_at": expires_at })),
        )
        .await?;
        tx.commit().await?;
        return Ok(NewCode {
            id,
            code,
            expires_at,
        });
    }
    Err(AssistError::NoFreeCode)
}

/// Consume `code` and issue a certificate for the CSR, for a new throwaway
/// agent. Single-use, like `enroll::enroll`. Audited as `assist.redeem`.
pub async fn redeem(
    pool: &PgPool,
    ca: &AgentCa,
    code: &str,
    csr_der: &[u8],
    remote: IpAddr,
) -> Result<Enrollment, AssistError> {
    let code = protocol::assist::normalize_code(code).ok_or(AssistError::InvalidCode)?;
    let mut tx = pool.begin().await?;
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT s.id, u.username FROM assist_sessions s JOIN users u ON u.id = s.user_id
         WHERE s.code_hash = $1 AND s.redeemed_at IS NULL AND s.expires_at > now()
         FOR UPDATE OF s",
    )
    .bind(token_hash(&code))
    .fetch_optional(&mut *tx)
    .await?;
    let (session_id, technician) = row.ok_or(AssistError::InvalidCode)?;

    let agent_id = new_agent_id();
    let cert_pem = ca.issue_for(&agent_id, csr_der, CERT_VALIDITY)?;
    let fingerprint = crate::quic::cert_fingerprint(
        &common::tls::certs_from_pem(&cert_pem).map_err(|e| CaError::Load(e.to_string()))?[0],
    );

    sqlx::query(
        "INSERT INTO agents (id, enrollment_state, cert_fingerprint, assist_session_id)
         VALUES ($1, 'enrolled', $2, $3)",
    )
    .bind(&agent_id)
    .bind(&fingerprint)
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    // The user is always asked, and `admin_set` keeps the reported device
    // kind from changing that.
    sqlx::query(
        "INSERT INTO device_policies
             (agent_id, consent_mode, on_no_user, consent_timeout_secs, admin_set)
         VALUES ($1, 'require', 'deny', $2, true)",
    )
    .bind(&agent_id)
    .bind(CONSENT_TIMEOUT_SECS)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE assist_sessions SET redeemed_at = now(), code_hash = NULL WHERE id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(format!("agent:{agent_id}"), Action::AssistRedeem)
            .target(&agent_id)
            .detail(json!({
                "assist_session_id": session_id,
                "technician": technician,
                "cert_fingerprint": fingerprint,
                "remote_ip": remote.to_canonical().to_string(),
            })),
    )
    .await?;
    tx.commit().await?;
    Ok(Enrollment {
        agent_id,
        cert_pem,
        fingerprint,
    })
}

/// Where a session is, for the technician waiting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The code has not been typed yet.
    Waiting,
    /// The code ran out, or was voided, before anyone typed it.
    Expired,
    /// The code was accepted; the client is not connected (yet, or just now).
    Connecting,
    /// The client is connected: a viewer can be started.
    Connected,
    /// The session is over and its agent is gone.
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub id: i64,
    pub status: State,
    pub expires_at: DateTime<Utc>,
    /// The session's agent, once the code has been accepted.
    pub agent_id: Option<String>,
    pub hostname: Option<String>,
}

#[derive(sqlx::FromRow)]
struct StatusRow {
    user_id: i64,
    usable: bool,
    expires_at: DateTime<Utc>,
    redeemed: bool,
    agent_id: Option<String>,
    hostname: Option<String>,
}

/// Session `id`, as `user` may see it: `None` if there is no such session
/// or it is someone else's (admins see them all).
pub async fn status(
    pool: &PgPool,
    hub: Option<&Hub>,
    id: i64,
    user: &User,
) -> sqlx::Result<Option<Status>> {
    let row: Option<StatusRow> = sqlx::query_as(
        "SELECT s.user_id, s.code_hash IS NOT NULL AND s.expires_at > now() AS usable,
                s.expires_at, s.redeemed_at IS NOT NULL AS redeemed,
                a.id AS agent_id, a.hostname
         FROM assist_sessions s LEFT JOIN agents a ON a.assist_session_id = s.id
         WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row.filter(|row| row.user_id == user.id || user.role == Role::Admin) else {
        return Ok(None);
    };
    let online = match (&row.agent_id, hub) {
        (Some(agent_id), Some(hub)) => hub.is_online(agent_id),
        _ => false,
    };
    let status = match (row.redeemed, &row.agent_id) {
        (true, Some(_)) if online => State::Connected,
        (true, Some(_)) => State::Connecting,
        (true, None) => State::Ended,
        (false, _) if row.usable => State::Waiting,
        (false, _) => State::Expired,
    };
    Ok(Some(Status {
        id,
        status,
        expires_at: row.expires_at,
        agent_id: row.agent_id,
        hostname: row.hostname,
    }))
}

/// Delete quick assist agents that have been disconnected for `gone_after`
/// (audited as `assist.end`), and forget old sessions. Returns the agents
/// deleted.
pub async fn prune(pool: &PgPool, gone_after: Duration) -> sqlx::Result<Vec<String>> {
    let mut tx = pool.begin().await?;
    let gone: Vec<(String, i64)> = sqlx::query_as(
        "DELETE FROM agents
         WHERE assist_session_id IS NOT NULL
           AND COALESCE(last_seen, created_at) < now() - make_interval(secs => $1)
         RETURNING id, assist_session_id",
    )
    .bind(gone_after.as_secs_f64())
    .fetch_all(&mut *tx)
    .await?;
    for (agent_id, session_id) in &gone {
        sqlx::query("UPDATE assist_sessions SET ended_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        audit::append(
            &mut tx,
            NewEntry::new("system", Action::AssistEnd)
                .target(agent_id)
                .detail(json!({ "assist_session_id": session_id })),
        )
        .await?;
    }
    sqlx::query(
        "DELETE FROM assist_sessions s
         WHERE s.created_at < now() - make_interval(secs => $1)
           AND NOT EXISTS (SELECT 1 FROM agents a WHERE a.assist_session_id = s.id)",
    )
    .bind(KEEP_SESSIONS.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(gone.into_iter().map(|(agent_id, _)| agent_id).collect())
}

/// Void every code not yet redeemed (technicians make new ones). Audited
/// as `assist.lockout`. Returns how many were voided.
pub async fn void_all(pool: &PgPool, failures: usize) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let voided = sqlx::query(
        "UPDATE assist_sessions SET code_hash = NULL, expires_at = LEAST(expires_at, now())
         WHERE code_hash IS NOT NULL",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit::append(
        &mut tx,
        NewEntry::new("system", Action::AssistLockout)
            .detail(json!({ "failed_attempts": failures, "codes_voided": voided })),
    )
    .await?;
    tx.commit().await?;
    Ok(voided)
}

/// Wrong codes one address may send within [`Limiter::WINDOW`].
pub const MAX_FAILURES_PER_IP: usize = 5;
/// Wrong codes from everywhere within [`Limiter::WINDOW`] before every
/// outstanding code is voided.
pub const MAX_FAILURES: usize = 100;

/// What to do about a wrong code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Counted,
    /// Too many from everywhere: void the outstanding codes
    /// ([`void_all`]). Carries how many failures there were.
    Lockout(usize),
}

#[derive(Default)]
struct Failures {
    by_ip: HashMap<IpAddr, VecDeque<Instant>>,
    all: VecDeque<Instant>,
}

/// Limits guessing of codes. In memory: a restart forgives everyone, which
/// costs a guesser more time than it saves.
#[derive(Default)]
pub struct Limiter {
    failures: Mutex<Failures>,
}

impl Limiter {
    /// How long a wrong code counts against its sender.
    pub const WINDOW: Duration = Duration::from_secs(10 * 60);

    fn failures(&self) -> std::sync::MutexGuard<'_, Failures> {
        self.failures.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expire(queue: &mut VecDeque<Instant>, now: Instant) {
        while queue
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Self::WINDOW)
        {
            queue.pop_front();
        }
    }

    /// Whether `ip` may try a code now.
    pub fn allows(&self, ip: IpAddr, now: Instant) -> bool {
        let mut failures = self.failures();
        let ip = ip.to_canonical();
        let Some(queue) = failures.by_ip.get_mut(&ip) else {
            return true;
        };
        Self::expire(queue, now);
        if queue.is_empty() {
            failures.by_ip.remove(&ip);
            return true;
        }
        queue.len() < MAX_FAILURES_PER_IP
    }

    /// `ip` sent a wrong code.
    pub fn failed(&self, ip: IpAddr, now: Instant) -> Failure {
        let mut failures = self.failures();
        failures
            .by_ip
            .entry(ip.to_canonical())
            .or_default()
            .push_back(now);
        // Addresses that stopped trying are forgotten.
        failures.by_ip.retain(|_, queue| {
            Self::expire(queue, now);
            !queue.is_empty()
        });
        Self::expire(&mut failures.all, now);
        failures.all.push_back(now);
        if failures.all.len() >= MAX_FAILURES {
            let count = failures.all.len();
            failures.all.clear();
            return Failure::Lockout(count);
        }
        Failure::Counted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..200 {
            let code = random_code();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
            assert_eq!(protocol::assist::normalize_code(&code), Some(code));
        }
        assert!(new_agent_id().starts_with("qa-"));
        assert_ne!(new_agent_id(), new_agent_id());
    }

    #[test]
    fn an_address_is_stopped_after_five_wrong_codes_until_they_age_out() {
        let limiter = Limiter::default();
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        let other: IpAddr = "203.0.113.10".parse().unwrap();
        let start = Instant::now();
        for i in 0..MAX_FAILURES_PER_IP {
            assert!(limiter.allows(ip, start), "attempt {i}");
            assert_eq!(limiter.failed(ip, start), Failure::Counted);
        }
        assert!(!limiter.allows(ip, start));
        assert!(limiter.allows(other, start));
        // The same address as IPv4-mapped IPv6 is the same address.
        assert!(!limiter.allows("::ffff:203.0.113.9".parse().unwrap(), start));
        let later = start + Limiter::WINDOW;
        assert!(limiter.allows(ip, later));
    }

    #[test]
    fn many_wrong_codes_from_everywhere_lock_out_and_start_over() {
        let limiter = Limiter::default();
        let start = Instant::now();
        let ip = |n: usize| -> IpAddr { format!("10.0.{}.{}", n / 250, n % 250).parse().unwrap() };
        for n in 0..MAX_FAILURES - 1 {
            assert_eq!(limiter.failed(ip(n), start), Failure::Counted);
        }
        assert_eq!(
            limiter.failed(ip(MAX_FAILURES), start),
            Failure::Lockout(MAX_FAILURES)
        );
        assert_eq!(
            limiter.failed(ip(MAX_FAILURES + 1), start),
            Failure::Counted
        );
        // Spread over more than the window, they never add up.
        let limiter = Limiter::default();
        for n in 0..MAX_FAILURES * 2 {
            let now = start + Limiter::WINDOW / 50 * n as u32;
            assert_eq!(limiter.failed(ip(n), now), Failure::Counted);
        }
    }
}
