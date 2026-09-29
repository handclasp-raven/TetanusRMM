//! The HTTPS API end to end: real TLS, real Postgres, real HTTP client.

mod support;

use reqwest::StatusCode;
use serde_json::{json, Value};
use server::audit::{self, Verification};
use server::users::Role;
use support::{create_user, current_code, insert_agent, start_api, start_db, wrong_code};

#[tokio::test]
async fn health_responds_over_https() {
    let db = start_db().await;
    let api = start_api(db.pool.clone()).await;
    let resp = api.client.get(api.url("/api/health")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({ "status": "ok" })
    );
}

#[tokio::test]
async fn login_flow_and_session_lifecycle() {
    let db = start_db().await;
    let alice = create_user(&db.pool, "alice", Role::SupportEngineer).await;
    let api = start_api(db.pool.clone()).await;

    // No token, bad token.
    let resp = api.client.get(api.url("/api/me")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = api
        .client
        .get(api.url("/api/me"))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Wrong password never yields a challenge.
    let resp = api
        .client
        .post(api.url("/api/auth/login"))
        .json(&json!({ "username": "alice", "password": "wrong password!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Right password, wrong code.
    let resp = api.login("alice", &wrong_code(&alice.totp_secret)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"],
        "invalid TOTP code"
    );

    // Right password, right code.
    let resp = api.login("alice", &current_code(&alice.totp_secret)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["username"], "alice");
    assert_eq!(body["user"]["role"], "support_engineer");
    let token = body["session_token"].as_str().unwrap();

    let me: Value = api
        .client
        .get(api.url("/api/me"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["username"], "alice");

    let resp = api
        .client
        .post(api.url("/api/auth/logout"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = api
        .client
        .get(api.url("/api/me"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(
        support::audit_actions(&db.pool).await,
        [
            "user.create",
            "login.bad_password",
            "login.bad_totp",
            "login.success",
            "logout"
        ]
    );
    assert_eq!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { entries: 5 }
    );
}

#[tokio::test]
async fn policies_are_admin_only_and_audited() {
    let db = start_db().await;
    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let auditor = create_user(&db.pool, "carol", Role::Auditor).await;
    insert_agent(&db.pool, "agent-1").await;
    let api = start_api(db.pool.clone()).await;
    let admin_token = api.session("root", &admin.totp_secret).await;
    let auditor_token = api.session("carol", &auditor.totp_secret).await;

    // Everyone can read.
    let agents: Value = api
        .client
        .get(api.url("/api/agents"))
        .bearer_auth(&auditor_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(agents[0]["id"], "agent-1");
    assert_eq!(agents[0]["enrollment_state"], "enrolled");
    let policy: Value = api
        .client
        .get(api.url("/api/agents/agent-1/policy"))
        .bearer_auth(&auditor_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        policy,
        json!({ "consent_mode": "notify", "on_no_user": "deny", "consent_timeout_secs": 30 })
    );

    let new_policy =
        json!({ "consent_mode": "require", "on_no_user": "allow", "consent_timeout_secs": 45 });
    let put = |token: &str, agent: &str, body: &Value| {
        api.client
            .put(api.url(&format!("/api/agents/{agent}/policy")))
            .bearer_auth(token)
            .json(body)
            .send()
    };

    // The auditor is read-only.
    let resp = put(&auditor_token, "agent-1", &new_policy).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Admin: validation, unknown agent, success.
    let mut bad = new_policy.clone();
    bad["consent_timeout_secs"] = json!(0);
    assert_eq!(
        put(&admin_token, "agent-1", &bad).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let mut bad_enum = new_policy.clone();
    bad_enum["consent_mode"] = json!("sometimes");
    assert_eq!(
        put(&admin_token, "agent-1", &bad_enum)
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        put(&admin_token, "ghost", &new_policy)
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let resp = put(&admin_token, "agent-1", &new_policy).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.json::<Value>().await.unwrap(), new_policy);

    let actions = support::audit_actions(&db.pool).await;
    assert_eq!(actions.iter().filter(|a| *a == "policy.update").count(), 1);
    assert_eq!(actions.last().unwrap(), "policy.update");
    assert!(matches!(
        audit::verify(&db.pool).await.unwrap(),
        Verification::Valid { .. }
    ));
}
