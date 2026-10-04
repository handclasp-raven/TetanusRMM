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

#[tokio::test]
async fn auditors_and_admins_read_and_verify_the_audit_log() {
    let db = start_db().await;
    let auditor = create_user(&db.pool, "reader", Role::Auditor).await;
    let engineer = create_user(&db.pool, "eng", Role::SupportEngineer).await;
    let api = start_api(db.pool.clone()).await;
    let reader = api.session("reader", &auditor.totp_secret).await;
    let eng = api.session("eng", &engineer.totp_secret).await;
    let get = |path: &str, token: &str| api.client.get(api.url(path)).bearer_auth(token).send();

    let entries: Vec<Value> = get("/api/audit?limit=2", &reader)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Newest first: the two logins just made.
    assert_eq!(entries.len(), 2);
    assert!(entries[0]["id"].as_i64() > entries[1]["id"].as_i64());
    assert_eq!(entries[0]["actor"], "eng");
    assert_eq!(entries[0]["action"], "login.success");
    assert_eq!(entries[0]["hash"].as_str().unwrap().len(), 64);

    let verified: Value = get("/api/audit/verify", &reader)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(verified["status"], "valid");
    assert!(verified["entries"].as_u64().unwrap() >= 4);

    assert_eq!(
        get("/api/audit?limit=0", &reader).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    for path in ["/api/audit", "/api/audit/verify"] {
        assert_eq!(
            get(path, &eng).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}

/// A PNG header saying `width` x `height`.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    out.extend_from_slice(&13u32.to_be_bytes());
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    out
}

#[tokio::test]
async fn branding_is_read_by_anyone_and_set_by_admins_only() {
    use protocol::brand::base64;
    let db = start_db().await;
    let api = start_api(db.pool.clone()).await;
    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let engineer = create_user(&db.pool, "jane", Role::SupportEngineer).await;
    let root = api.session("root", &admin.totp_secret).await;
    let jane = api.session("jane", &engineer.totp_secret).await;
    let url = api.url("/api/branding");

    // Nothing set: null, without signing in.
    let get = || async {
        let resp = api.client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json::<Value>().await.unwrap()
    };
    assert_eq!(get().await, Value::Null);

    let logo = base64::encode(&png(128, 96));
    let contoso = json!({ "name": "  Contoso IT ", "accent": "#0b5cad", "logo_png": logo });
    let put = |token: &str, body: Value| {
        let request = api.client.put(&url).bearer_auth(token).json(&body);
        async move { request.send().await.unwrap() }
    };
    // Not signed in, and not an admin: refused, and nothing changes.
    let anonymous = api.client.put(&url).json(&contoso).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        put(&jane, contoso.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(get().await, Value::Null);

    let saved = put(&root, contoso.clone()).await;
    assert_eq!(saved.status(), StatusCode::OK);
    let expected = json!({ "name": "Contoso IT", "accent": "#0B5CAD", "logo_png": logo });
    assert_eq!(saved.json::<Value>().await.unwrap(), expected);
    assert_eq!(get().await, expected);

    // What cannot be shown is refused with the reason, and the last one stays.
    for (body, why) in [
        (json!({ "name": "" }), "name"),
        (json!({ "name": "x".repeat(49) }), "name"),
        (json!({ "name": "Contoso", "accent": "blue" }), "#RRGGBB"),
        (
            json!({ "name": "Contoso", "accent": "#FFEE00" }),
            "too light",
        ),
        (json!({ "name": "Contoso", "logo_png": "***" }), "base64"),
        (
            json!({ "name": "Contoso", "logo_png": base64::encode(b"GIF89a") }),
            "PNG",
        ),
        (
            json!({ "name": "Contoso", "logo_png": base64::encode(&png(2048, 64)) }),
            "2048x64",
        ),
    ] {
        let resp = put(&root, body.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{body}");
        let error = resp.json::<Value>().await.unwrap()["error"].to_string();
        assert!(error.contains(why), "{body}: {error}");
    }
    assert_eq!(get().await, expected);

    // A name alone is a branding too; it replaces the whole of the last.
    let name_only = put(&root, json!({ "name": "Contoso" })).await;
    assert_eq!(
        name_only.json::<Value>().await.unwrap(),
        json!({ "name": "Contoso", "accent": null, "logo_png": null })
    );

    // Reset: admins only, and back to null.
    let delete = |token: &str| {
        let request = api.client.delete(&url).bearer_auth(token);
        async move { request.send().await.unwrap().status() }
    };
    assert_eq!(delete(&jane).await, StatusCode::FORBIDDEN);
    assert_eq!(delete(&root).await, StatusCode::OK);
    assert_eq!(get().await, Value::Null);
    assert_eq!(delete(&root).await, StatusCode::OK);

    // Audited: two sets and one reset, by the admin, without the logo.
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT actor, detail FROM audit_log WHERE action = 'branding.update' ORDER BY id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (
                "root".to_owned(),
                json!({ "name": "Contoso IT", "accent": "#0B5CAD", "logo": true })
            ),
            (
                "root".to_owned(),
                json!({ "name": "Contoso", "accent": null, "logo": false })
            ),
            ("root".to_owned(), json!({ "reset": true })),
        ]
    );
}
