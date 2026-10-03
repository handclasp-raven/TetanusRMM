//! Hash-chained, tamper-evident audit log.
//!
//! Each row stores `hash = SHA-256(prev_hash || canonical(row without hash))`,
//! where `prev_hash` is the previous row's `hash` (32 zero bytes for row 1).
//! Changing any field of any row, or deleting or reordering rows, breaks the
//! chain at that point, and [`verify`] reports where.
//!
//! What this does NOT catch on its own: deleting rows from the *end* of the
//! log, or rewriting the whole chain from some row onwards. Both need the
//! latest hash to be anchored somewhere outside the database.

use chrono::{DateTime, TimeZone, Utc};
use futures_util::TryStreamExt;
use protocol::credential::CredentialEvent;
use ring::digest::{digest, SHA256};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

/// Bumped if the canonical encoding ever changes, so old rows stay verifiable.
const CANONICAL_VERSION: u32 = 1;

/// Arbitrary constant key for the advisory lock that serialises appends.
const APPEND_LOCK_KEY: i64 = 0x524d_4d5f_4155_4454; // "RMM_AUDT"

pub const GENESIS_HASH: [u8; 32] = [0; 32];

/// Everything that gets audited. Stored as the `action` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    UserCreate,
    LoginSuccess,
    LoginBadPassword,
    LoginBadTotp,
    Logout,
    PolicyUpdate,
    EnrollmentCreate,
    AgentEnroll,
    ViewerSessionCreate,
    ViewerConnect,
    ViewerDisconnect,
    /// The device's reported kind applied its default consent mode.
    PolicyDefault,
    /// A viewer session's consent was decided; detail has mode and outcome.
    SessionStart,
    /// The user ended a session with the Ctrl+F12 kill switch.
    SessionUserTerminated,
    /// A session moved between the relay and a direct path, or a direct
    /// attempt failed; detail has the path, never content.
    SessionPath,
    /// A remote operation was refused because of the user's role.
    PermissionDenied,
    /// An interactive shell was requested; detail says whether it started.
    ShellOpen,
    /// An interactive shell ended; detail has the duration and byte counts.
    ShellClose,
    /// A script was sent to agents; detail has a summary and its hash.
    ScriptRun,
    /// A script run finished; detail has each agent's exit code.
    ScriptComplete,
    FileUpload,
    FileDownload,
    /// An admin changed a user's role.
    UserRoleChange,
    UserDelete,
    /// An admin set a user's password; their sessions were ended.
    UserPasswordChange,
    /// An admin replaced a user's TOTP secret; their sessions were ended.
    UserTotpReset,
    /// Access granted to a support engineer; detail has scope and capabilities.
    GrantCreate,
    GrantDelete,
    GroupCreate,
    /// A group was renamed or redescribed.
    GroupUpdate,
    GroupDelete,
    /// Agents were added to or removed from a group.
    GroupMembers,
    /// An admin set or cleared an agent's classification.
    AgentClassify,
    /// A program was started on an agent's desktop; detail has the command
    /// and how it went.
    CommandLaunch,
    /// A technician made a quick assist code.
    AssistCreate,
    /// A quick assist code was typed; detail has the throwaway agent.
    AssistRedeem,
    /// A quick assist agent was removed after its session.
    AssistEnd,
    /// Too many wrong quick assist codes: the outstanding ones were voided.
    AssistLockout,
    /// Something happened to the password a user lent the technicians on
    /// an agent (asked for, stored, typed, forgotten...). Never the
    /// password, which stays on the agent.
    Credential(CredentialEvent),
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::UserCreate => "user.create",
            Action::LoginSuccess => "login.success",
            Action::LoginBadPassword => "login.bad_password",
            Action::LoginBadTotp => "login.bad_totp",
            Action::Logout => "logout",
            Action::PolicyUpdate => "policy.update",
            Action::EnrollmentCreate => "enrollment.create",
            Action::AgentEnroll => "agent.enroll",
            Action::ViewerSessionCreate => "viewer.session_create",
            Action::ViewerConnect => "viewer.connect",
            Action::ViewerDisconnect => "viewer.disconnect",
            Action::PolicyDefault => "policy.default",
            Action::SessionStart => "session.start",
            Action::SessionUserTerminated => "session.user_terminated",
            Action::SessionPath => "session.path",
            Action::PermissionDenied => "permission.denied",
            Action::ShellOpen => "shell.open",
            Action::ShellClose => "shell.close",
            Action::ScriptRun => "script.run",
            Action::ScriptComplete => "script.complete",
            Action::FileUpload => "file.upload",
            Action::FileDownload => "file.download",
            Action::UserRoleChange => "user.role_change",
            Action::UserDelete => "user.delete",
            Action::UserPasswordChange => "user.password_change",
            Action::UserTotpReset => "user.totp_reset",
            Action::GrantCreate => "grant.create",
            Action::GrantDelete => "grant.delete",
            Action::GroupCreate => "group.create",
            Action::GroupUpdate => "group.update",
            Action::GroupDelete => "group.delete",
            Action::GroupMembers => "group.members",
            Action::AgentClassify => "agent.classify",
            Action::CommandLaunch => "command.launch",
            Action::AssistCreate => "assist.create",
            Action::AssistRedeem => "assist.redeem",
            Action::AssistEnd => "assist.end",
            Action::AssistLockout => "assist.lockout",
            Action::Credential(CredentialEvent::Requested) => "credential.requested",
            Action::Credential(CredentialEvent::Stored) => "credential.stored",
            Action::Credential(CredentialEvent::Declined) => "credential.declined",
            Action::Credential(CredentialEvent::Unavailable) => "credential.unavailable",
            Action::Credential(CredentialEvent::Typed) => "credential.typed",
            Action::Credential(CredentialEvent::Forgotten) => "credential.forgotten",
            Action::Credential(CredentialEvent::NotStored) => "credential.not_stored",
        }
    }
}

/// A row to append.
#[derive(Debug, Clone)]
pub struct NewEntry {
    pub actor: String,
    pub action: Action,
    pub target: Option<String>,
    pub detail: Value,
}

impl NewEntry {
    pub fn new(actor: impl Into<String>, action: Action) -> Self {
        Self {
            actor: actor.into(),
            action,
            target: None,
            detail: Value::Object(Default::default()),
        }
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }
}

/// A stored row.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Entry {
    pub id: i64,
    pub ts: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub detail: Value,
    #[serde(with = "hex_bytes")]
    pub prev_hash: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub hash: Vec<u8>,
}

impl Entry {
    /// The hash this row should have given its contents and `prev_hash`.
    pub fn compute_hash(&self) -> [u8; 32] {
        compute_hash(
            &self.prev_hash,
            self.id,
            self.ts,
            &self.actor,
            &self.action,
            self.target.as_deref(),
            &self.detail,
        )
    }
}

/// Result of walking the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verification {
    Valid { entries: u64 },
    Broken { id: i64, reason: BreakReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakReason {
    /// Row ids are not 1, 2, 3, ...: a row was deleted or inserted out of band.
    /// `id` is the first unexpected id.
    SequenceGap,
    /// `prev_hash` does not match the previous row's `hash`.
    PrevHashMismatch,
    /// The row's contents do not hash to its stored `hash`.
    HashMismatch,
}

/// Append an entry inside `tx`.
///
/// Callers pass the same transaction that performs the audited change, so the
/// change and its audit row commit or roll back together.
pub async fn append(tx: &mut Transaction<'_, Postgres>, entry: NewEntry) -> sqlx::Result<Entry> {
    // Serialise appends so two writers cannot both chain onto the same row.
    // Released automatically at commit/rollback.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(APPEND_LOCK_KEY)
        .execute(&mut **tx)
        .await?;

    let last: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT id, hash FROM audit_log ORDER BY id DESC LIMIT 1")
            .fetch_optional(&mut **tx)
            .await?;
    let (id, prev_hash) = match last {
        Some((id, hash)) => (id + 1, hash),
        None => (1, GENESIS_HASH.to_vec()),
    };

    let mut row = Entry {
        id,
        ts: now_micros(),
        actor: entry.actor,
        action: entry.action.as_str().to_owned(),
        target: entry.target,
        detail: entry.detail,
        prev_hash,
        hash: Vec::new(),
    };
    row.hash = row.compute_hash().to_vec();

    sqlx::query(
        "INSERT INTO audit_log (id, ts, actor, action, target, detail, prev_hash, hash)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(row.id)
    .bind(row.ts)
    .bind(&row.actor)
    .bind(&row.action)
    .bind(&row.target)
    .bind(&row.detail)
    .bind(&row.prev_hash)
    .bind(&row.hash)
    .execute(&mut **tx)
    .await?;
    // Counted once written; a rolled-back transaction over-counts, rarely.
    crate::metrics::get().audit_entries.inc();
    Ok(row)
}

/// Append an entry in its own transaction.
pub async fn append_now(pool: &PgPool, entry: NewEntry) -> sqlx::Result<Entry> {
    let mut tx = pool.begin().await?;
    let row = append(&mut tx, entry).await?;
    tx.commit().await?;
    Ok(row)
}

/// The newest `limit` entries, newest first.
pub async fn recent(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Entry>> {
    sqlx::query_as(
        "SELECT id, ts, actor, action, target, detail, prev_hash, hash
         FROM audit_log ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Walk the whole chain in id order and report the first break, if any.
pub async fn verify(pool: &PgPool) -> sqlx::Result<Verification> {
    let mut rows = sqlx::query_as::<_, Entry>(
        "SELECT id, ts, actor, action, target, detail, prev_hash, hash
         FROM audit_log ORDER BY id",
    )
    .fetch(pool);

    let mut expected_id = 1i64;
    let mut prev_hash = GENESIS_HASH.to_vec();
    while let Some(row) = rows.try_next().await? {
        if let Some(reason) = check_link(&row, expected_id, &prev_hash) {
            return Ok(Verification::Broken { id: row.id, reason });
        }
        expected_id += 1;
        prev_hash = row.hash;
    }
    Ok(Verification::Valid {
        entries: (expected_id - 1) as u64,
    })
}

/// Check one row against the expected id and the previous row's hash.
pub fn check_link(row: &Entry, expected_id: i64, prev_hash: &[u8]) -> Option<BreakReason> {
    if row.id != expected_id {
        Some(BreakReason::SequenceGap)
    } else if row.prev_hash != prev_hash {
        Some(BreakReason::PrevHashMismatch)
    } else if row.hash != row.compute_hash() {
        Some(BreakReason::HashMismatch)
    } else {
        None
    }
}

/// Postgres stores microseconds; truncate so the hashed value survives a round trip.
fn now_micros() -> DateTime<Utc> {
    Utc.timestamp_micros(Utc::now().timestamp_micros())
        .single()
        .expect("current time is representable")
}

fn compute_hash(
    prev_hash: &[u8],
    id: i64,
    ts: DateTime<Utc>,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: &Value,
) -> [u8; 32] {
    let mut input = Vec::with_capacity(prev_hash.len() + 256);
    input.extend_from_slice(prev_hash);
    input.extend_from_slice(canonical(id, ts, actor, action, target, detail).as_bytes());
    digest(&SHA256, &input)
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// Canonical encoding of a row without its hash: a JSON array
/// `[version, id, ts_micros, actor, action, target, detail]` with object keys
/// sorted and no whitespace.
///
/// Keys are sorted explicitly rather than relying on serde_json's map order,
/// because Postgres `jsonb` does not preserve key order and serde_json's order
/// changes if any crate in the build enables its `preserve_order` feature.
pub fn canonical(
    id: i64,
    ts: DateTime<Utc>,
    actor: &str,
    action: &str,
    target: Option<&str>,
    detail: &Value,
) -> String {
    let fields = Value::Array(vec![
        CANONICAL_VERSION.into(),
        id.into(),
        ts.timestamp_micros().into(),
        actor.into(),
        action.into(),
        target.map_or(Value::Null, Into::into),
        detail.clone(),
    ]);
    let mut out = String::new();
    write_canonical(&fields, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            out.push_str(&value.to_string());
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
    }
}

mod hex_bytes {
    pub fn serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: i64, prev_hash: Vec<u8>) -> Entry {
        let mut e = Entry {
            id,
            ts: Utc.timestamp_micros(1_790_000_000_123_456).unwrap(),
            actor: "alice".into(),
            action: Action::PolicyUpdate.as_str().into(),
            target: Some("agent-1".into()),
            detail: json!({"b": 1, "a": {"y": [1, 2], "x": null}}),
            prev_hash,
            hash: Vec::new(),
        };
        e.hash = e.compute_hash().to_vec();
        e
    }

    #[test]
    fn canonical_form_is_compact_and_sorted() {
        let e = entry(7, GENESIS_HASH.to_vec());
        assert_eq!(
            canonical(
                e.id,
                e.ts,
                &e.actor,
                &e.action,
                e.target.as_deref(),
                &e.detail
            ),
            r#"[1,7,1790000000123456,"alice","policy.update","agent-1",{"a":{"x":null,"y":[1,2]},"b":1}]"#
        );
    }

    #[test]
    fn hash_ignores_json_key_order() {
        let a = entry(1, GENESIS_HASH.to_vec());
        let mut b = a.clone();
        b.detail = serde_json::from_str(r#"{"a": {"x": null, "y": [1, 2]}, "b": 1}"#).unwrap();
        assert_eq!(a.compute_hash(), b.compute_hash());
    }

    #[test]
    fn every_field_affects_the_hash() {
        let base = entry(1, GENESIS_HASH.to_vec());
        let original = base.compute_hash();
        let mutations: Vec<fn(&mut Entry)> = vec![
            |e| e.id += 1,
            |e| e.ts += chrono::Duration::microseconds(1),
            |e| e.actor.push('x'),
            |e| e.action.push('x'),
            |e| e.target = None,
            |e| e.detail["b"] = json!(2),
            |e| e.prev_hash[0] ^= 1,
        ];
        for (i, mutate) in mutations.into_iter().enumerate() {
            let mut e = base.clone();
            mutate(&mut e);
            assert_ne!(
                e.compute_hash(),
                original,
                "mutation {i} did not change the hash"
            );
        }
    }

    #[test]
    fn target_none_and_empty_string_hash_differently() {
        let a = entry(1, GENESIS_HASH.to_vec());
        let mut b = a.clone();
        b.target = Some(String::new());
        let mut c = a.clone();
        c.target = None;
        assert_ne!(b.compute_hash(), c.compute_hash());
    }

    #[test]
    fn check_link_classifies_breaks() {
        let first = entry(1, GENESIS_HASH.to_vec());
        let second = entry(2, first.hash.clone());
        assert_eq!(check_link(&first, 1, &GENESIS_HASH), None);
        assert_eq!(check_link(&second, 2, &first.hash), None);

        assert_eq!(
            check_link(&second, 1, &GENESIS_HASH),
            Some(BreakReason::SequenceGap)
        );
        assert_eq!(
            check_link(&second, 2, &GENESIS_HASH),
            Some(BreakReason::PrevHashMismatch)
        );
        let mut tampered = second.clone();
        tampered.actor = "mallory".into();
        assert_eq!(
            check_link(&tampered, 2, &first.hash),
            Some(BreakReason::HashMismatch)
        );
    }
}
