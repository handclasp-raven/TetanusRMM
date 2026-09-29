//! Users, login, audit chain and registry against a real Postgres.

mod support;

use serde_json::json;
use server::audit::{self, Action, BreakReason, Entry, NewEntry, Verification};
use server::auth::{self, AuthSettings, LoginError, MAX_TOTP_ATTEMPTS};
use server::registry::{self, ConsentMode, DevicePolicy, OnNoUser, PolicyError};
use server::users::{self, CreateUserError, Role};
use sqlx::PgPool;
use support::{
    audit_actions, create_user, current_code, insert_agent, start_db, wrong_code, PASSWORD,
};

async fn login_code(
    pool: &PgPool,
    username: &str,
    code: &str,
) -> Result<auth::SessionGrant, LoginError> {
    let challenge = auth::login_password(pool, username, PASSWORD).await?;
    auth::login_totp(
        pool,
        &AuthSettings::default(),
        &challenge.challenge_token,
        code,
    )
    .await
}

#[tokio::test]
async fn create_user_stores_argon2id_hash_and_is_audited() {
    let db = start_db().await;
    let created = create_user(&db.pool, "alice", Role::SupportEngineer).await;
    assert_eq!(created.user.username, "alice");
    assert_eq!(created.user.role, Role::SupportEngineer);

    let (hash, secret): (String, String) =
        sqlx::query_as("SELECT password_hash, totp_secret FROM users WHERE username = 'alice'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(hash.starts_with("$argon2id$v=19$"), "{hash}");
    assert!(!hash.contains(PASSWORD));
    assert_eq!(secret, created.totp_secret);
    assert!(created.otpauth_url.starts_with("otpauth://totp/RMM:alice?"));

    assert_eq!(audit_actions(&db.pool).await, ["user.create"]);
}

#[tokio::test]
async fn create_user_rejects_duplicates_and_weak_passwords() {
    let db = start_db().await;
    create_user(&db.pool, "alice", Role::Admin).await;

    let dup = users::create_user(&db.pool, "test", "alice", PASSWORD, Role::Admin).await;
    assert!(matches!(dup, Err(CreateUserError::Duplicate)), "{dup:?}");
    let weak = users::create_user(&db.pool, "test", "bob", "short", Role::Admin).await;
    assert!(
        matches!(weak, Err(CreateUserError::WeakPassword)),
        "{weak:?}"
    );
    let bad_name = users::create_user(&db.pool, "test", "bad name", PASSWORD, Role::Admin).await;
    assert!(matches!(bad_name, Err(CreateUserError::InvalidUsername)));

    // Failed creations leave no trace in the audit log.
    assert_eq!(audit_actions(&db.pool).await, ["user.create"]);
}

#[tokio::test]
async fn login_with_correct_totp_opens_a_session() {
    let db = start_db().await;
    let user = create_user(&db.pool, "alice", Role::Admin).await;

    let challenge = auth::login_password(&db.pool, "alice", PASSWORD)
        .await
        .unwrap();
    // The challenge token is not a session.
    assert_eq!(
        auth::authenticate(&db.pool, &challenge.challenge_token)
            .await
            .unwrap(),
        None
    );

    let grant = auth::login_totp(
        &db.pool,
        &AuthSettings::default(),
        &challenge.challenge_token,
        &current_code(&user.totp_secret),
    )
    .await
    .unwrap();
    assert_eq!(grant.user, user.user);
    assert_ne!(grant.token, challenge.challenge_token);
    assert_eq!(
        auth::authenticate(&db.pool, &grant.token).await.unwrap(),
        Some(user.user.clone())
    );

    // The challenge is single-use.
    let again = auth::login_totp(
        &db.pool,
        &AuthSettings::default(),
        &challenge.challenge_token,
        &current_code(&user.totp_secret),
    )
    .await;
    assert!(
        matches!(again, Err(LoginError::InvalidChallenge)),
        "{again:?}"
    );

    assert_eq!(
        audit_actions(&db.pool).await,
        ["user.create", "login.success"]
    );

    auth::logout(&db.pool, &grant.user, &grant.token)
        .await
        .unwrap();
    assert_eq!(
        auth::authenticate(&db.pool, &grant.token).await.unwrap(),
        None
    );
    assert_eq!(audit_actions(&db.pool).await.last().unwrap(), "logout");
}

#[tokio::test]
async fn login_with_wrong_totp_is_rejected() {
    let db = start_db().await;
    let user = create_user(&db.pool, "alice", Role::Admin).await;

    let challenge = auth::login_password(&db.pool, "alice", PASSWORD)
        .await
        .unwrap();
    let result = auth::login_totp(
        &db.pool,
        &AuthSettings::default(),
        &challenge.challenge_token,
        &wrong_code(&user.totp_secret),
    )
    .await;
    assert!(matches!(result, Err(LoginError::InvalidTotp)), "{result:?}");

    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE stage = 'active'")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(active, 0);

    let detail: serde_json::Value =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'login.bad_totp'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(detail["reason"], "wrong_code");
    assert_eq!(detail["attempts"], 1);

    // A single mistake does not burn the challenge.
    auth::login_totp(
        &db.pool,
        &AuthSettings::default(),
        &challenge.challenge_token,
        &current_code(&user.totp_secret),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn totp_code_cannot_be_replayed() {
    let db = start_db().await;
    let user = create_user(&db.pool, "alice", Role::Admin).await;
    let code = current_code(&user.totp_secret);

    login_code(&db.pool, "alice", &code).await.unwrap();
    let replay = login_code(&db.pool, "alice", &code).await;
    assert!(matches!(replay, Err(LoginError::InvalidTotp)), "{replay:?}");

    let reason: String = sqlx::query_scalar(
        "SELECT detail->>'reason' FROM audit_log WHERE action = 'login.bad_totp'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(reason, "code_reused");
}

#[tokio::test]
async fn challenge_is_discarded_after_too_many_wrong_codes() {
    let db = start_db().await;
    let user = create_user(&db.pool, "alice", Role::Admin).await;
    let challenge = auth::login_password(&db.pool, "alice", PASSWORD)
        .await
        .unwrap();
    let wrong = wrong_code(&user.totp_secret);

    for _ in 0..MAX_TOTP_ATTEMPTS {
        let r = auth::login_totp(
            &db.pool,
            &AuthSettings::default(),
            &challenge.challenge_token,
            &wrong,
        )
        .await;
        assert!(matches!(r, Err(LoginError::InvalidTotp)), "{r:?}");
    }
    // Even the right code is refused now; the user must re-enter their password.
    let r = auth::login_totp(
        &db.pool,
        &AuthSettings::default(),
        &challenge.challenge_token,
        &current_code(&user.totp_secret),
    )
    .await;
    assert!(matches!(r, Err(LoginError::InvalidChallenge)), "{r:?}");
}

#[tokio::test]
async fn wrong_password_and_unknown_user_look_the_same() {
    let db = start_db().await;
    create_user(&db.pool, "alice", Role::Admin).await;

    let wrong = auth::login_password(&db.pool, "alice", "not the password").await;
    let unknown = auth::login_password(&db.pool, "mallory", PASSWORD).await;
    assert!(matches!(wrong, Err(LoginError::InvalidCredentials)));
    assert!(matches!(unknown, Err(LoginError::InvalidCredentials)));

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, detail->>'reason' FROM audit_log
         WHERE action = 'login.bad_password' ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("alice".to_string(), "wrong_password".to_string()),
            ("mallory".to_string(), "unknown_user".to_string())
        ]
    );
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

#[tokio::test]
async fn expired_session_is_rejected() {
    let db = start_db().await;
    let user = create_user(&db.pool, "alice", Role::Admin).await;
    let grant = login_code(&db.pool, "alice", &current_code(&user.totp_secret))
        .await
        .unwrap();

    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        auth::authenticate(&db.pool, &grant.token).await.unwrap(),
        None
    );
}

/// Build a realistic chain: user creation, logins, a policy change, a logout.
async fn populate_chain(pool: &PgPool) {
    let user = create_user(pool, "alice", Role::Admin).await;
    let _ = auth::login_password(pool, "alice", "not the password").await;
    let grant = login_code(pool, "alice", &current_code(&user.totp_secret))
        .await
        .unwrap();
    insert_agent(pool, "agent-1").await;
    registry::update_policy(
        pool,
        "alice",
        "agent-1",
        DevicePolicy {
            consent_mode: ConsentMode::Require,
            on_no_user: OnNoUser::Allow,
            consent_timeout_secs: 60,
        },
    )
    .await
    .unwrap();
    audit::append_now(
        pool,
        NewEntry::new("alice", Action::PolicyUpdate)
            .target("agent-1")
            .detail(json!({"unicode": "héllo ✓", "nested": {"z": [1.5, -2, null], "a": true}})),
    )
    .await
    .unwrap();
    auth::logout(pool, &grant.user, &grant.token).await.unwrap();
}

#[tokio::test]
async fn audit_chain_verifies() {
    let db = start_db().await;
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { entries: 0 }
    );

    populate_chain(&db.pool).await;
    assert_eq!(
        audit_actions(&db.pool).await,
        [
            "user.create",
            "login.bad_password",
            "login.success",
            "policy.update",
            "policy.update",
            "logout"
        ]
    );
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { entries: 6 }
    );

    // Row 1 chains from the genesis hash, every later row from its predecessor.
    let hashes: Vec<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT prev_hash, hash FROM audit_log ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(hashes[0].0, audit::GENESIS_HASH);
    for pair in hashes.windows(2) {
        assert_eq!(pair[1].0, pair[0].1);
    }
}

#[tokio::test]
async fn concurrent_appends_keep_the_chain_intact() {
    let db = start_db().await;
    let tasks: Vec<_> = (0..20)
        .map(|i| {
            let pool = db.pool.clone();
            tokio::spawn(async move {
                audit::append_now(&pool, NewEntry::new(format!("actor-{i}"), Action::Logout))
                    .await
                    .unwrap()
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { entries: 20 }
    );
}

async fn fetch_entry(pool: &PgPool, id: i64) -> Entry {
    sqlx::query_as("SELECT * FROM audit_log WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn mutated_audit_row_is_detected() {
    let db = start_db().await;
    populate_chain(&db.pool).await;
    let original = fetch_entry(&db.pool, 4).await;
    assert_eq!(original.action, "policy.update");

    // 1. Edit a field in place: row 4 no longer matches its hash.
    sqlx::query("UPDATE audit_log SET detail = jsonb_set(detail, '{after,consent_mode}', '\"unattended\"') WHERE id = 4")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Broken {
            id: 4,
            reason: BreakReason::HashMismatch
        }
    );

    // Restoring the original content makes the chain valid again.
    sqlx::query("UPDATE audit_log SET detail = $1 WHERE id = 4")
        .bind(&original.detail)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { .. }
    ));

    // 2. Edit a field AND recompute that row's hash: row 5 no longer links to it.
    let mut forged = original.clone();
    forged.actor = "mallory".into();
    forged.hash = forged.compute_hash().to_vec();
    sqlx::query("UPDATE audit_log SET actor = $1, hash = $2 WHERE id = 4")
        .bind(&forged.actor)
        .bind(&forged.hash)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Broken {
            id: 5,
            reason: BreakReason::PrevHashMismatch
        }
    );
    sqlx::query("UPDATE audit_log SET actor = $1, hash = $2 WHERE id = 4")
        .bind(&original.actor)
        .bind(&original.hash)
        .execute(&db.pool)
        .await
        .unwrap();

    // 3. Delete a row from the middle.
    sqlx::query("DELETE FROM audit_log WHERE id = 4")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Broken {
            id: 5,
            reason: BreakReason::SequenceGap
        }
    );
}

#[tokio::test]
async fn policy_update_validates_and_audits_before_and_after() {
    let db = start_db().await;
    insert_agent(&db.pool, "agent-1").await;
    let default = registry::get_policy(&db.pool, "agent-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        default,
        DevicePolicy {
            consent_mode: ConsentMode::Notify,
            on_no_user: OnNoUser::Deny,
            consent_timeout_secs: 30
        }
    );

    let new = DevicePolicy {
        consent_mode: ConsentMode::Unattended,
        ..default
    };
    let missing = registry::update_policy(&db.pool, "alice", "nope", new).await;
    assert!(matches!(missing, Err(PolicyError::NotFound)));
    let invalid = registry::update_policy(
        &db.pool,
        "alice",
        "agent-1",
        DevicePolicy {
            consent_timeout_secs: 0,
            ..new
        },
    )
    .await;
    assert!(matches!(invalid, Err(PolicyError::InvalidTimeout)));

    registry::update_policy(&db.pool, "alice", "agent-1", new)
        .await
        .unwrap();
    assert_eq!(
        registry::get_policy(&db.pool, "agent-1").await.unwrap(),
        Some(new)
    );

    let (actor, target, detail): (String, String, serde_json::Value) = sqlx::query_as(
        "SELECT actor, target, detail FROM audit_log WHERE action = 'policy.update'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(actor, "alice");
    assert_eq!(target, "agent-1");
    assert_eq!(detail["before"]["consent_mode"], "notify");
    assert_eq!(detail["after"]["consent_mode"], "unattended");
}

#[tokio::test]
async fn upgrading_to_rbac_keeps_existing_engineers_access() {
    // A database from before RBAC, with an engineer and an agent in it.
    let db = support::start_db_unmigrated().await;
    server::db::MIGRATOR
        .run_to(20260929000005, &db.pool)
        .await
        .unwrap();
    let engineer = create_user(&db.pool, "old-hand", Role::SupportEngineer).await;
    create_user(&db.pool, "reader", Role::Auditor).await;
    support::insert_agent(&db.pool, "agt-1").await;

    server::db::MIGRATOR.run(&db.pool).await.unwrap();

    // The engineer can still do everything everywhere (until an admin
    // narrows it); nobody else got a grant.
    let visibility = server::access::visibility(&db.pool, &engineer.user)
        .await
        .unwrap();
    assert_eq!(
        visibility.capabilities("agt-1"),
        server::access::all_capabilities()
    );
    let grants = server::access::list_grants(&db.pool, None).await.unwrap();
    assert_eq!(grants.len(), 1);
    assert!(grants[0].all_agents);
    assert_eq!(grants[0].created_by, "migration");

    // Engineers created from now on start with nothing.
    let newcomer = create_user(&db.pool, "newcomer", Role::SupportEngineer).await;
    assert_eq!(
        server::access::visibility(&db.pool, &newcomer.user)
            .await
            .unwrap(),
        server::access::Visibility::Only(Default::default())
    );
}
