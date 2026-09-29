//! RBAC end to end: roles, per-agent and per-group access grants, agent
//! groups, and user administration, through the real HTTPS API.
//!
//! Most agents here exist only in the registry (not connected), which is
//! enough: access is decided before the server contacts an agent, so an
//! allowed operation on an offline agent is a 409 (or an "offline" result)
//! and a refused one a 403.

mod support;

use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use server::users::{CreatedUser, Role};
use support::{audit_actions, create_user, insert_agent, start_api_full, start_db, Api};

struct Rig {
    db: support::TestDb,
    api: Api,
    admin: String,
    eng: String,
    eng_user: CreatedUser,
    auditor: String,
}

async fn rig() -> Rig {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    // An empty relay: every agent is offline.
    let hub = server::relay::Hub::new();
    let api = start_api_full(db.pool.clone(), &certs, "/nonexistent".into(), Some(hub)).await;
    for id in ["a1", "a2", "a3", "a4"] {
        insert_agent(&db.pool, id).await;
    }
    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let eng_user = create_user(&db.pool, "eng", Role::SupportEngineer).await;
    let auditor = create_user(&db.pool, "reader", Role::Auditor).await;
    Rig {
        admin: api.session("root", &admin.totp_secret).await,
        eng: api.session("eng", &eng_user.totp_secret).await,
        auditor: api.session("reader", &auditor.totp_secret).await,
        eng_user,
        db,
        api,
    }
}

impl Rig {
    async fn call(
        &self,
        method: Method,
        token: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self
            .api
            .client
            .request(method, self.api.url(path))
            .bearer_auth(token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let body = resp.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn get(&self, token: &str, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, token, path, None).await
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, token, path, Some(body)).await
    }

    /// Agent id -> (capabilities, groups), as `token` sees the list.
    async fn agents(&self, token: &str) -> Vec<(String, Vec<String>, Vec<String>)> {
        let (status, list) = self.get(token, "/api/agents").await;
        assert_eq!(status, StatusCode::OK);
        let strings = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect()
        };
        list.as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["id"].as_str().unwrap().to_owned(),
                    strings(&a["capabilities"]),
                    strings(&a["groups"]),
                )
            })
            .collect()
    }

    /// The details of `permission.denied` audit rows, oldest first.
    async fn denials(&self) -> Vec<Value> {
        sqlx::query_scalar(
            "SELECT detail FROM audit_log WHERE action = 'permission.denied' ORDER BY id",
        )
        .fetch_all(&self.db.pool)
        .await
        .unwrap()
    }
}

fn row(id: &str, caps: &[&str], groups: &[&str]) -> (String, Vec<String>, Vec<String>) {
    (
        id.into(),
        caps.iter().map(|s| s.to_string()).collect(),
        groups.iter().map(|s| s.to_string()).collect(),
    )
}

#[tokio::test]
async fn engineers_see_and_act_only_where_their_grants_allow() {
    let rig = rig().await;
    let eng_id = rig.eng_user.user.id;

    // A new engineer starts with nothing.
    assert_eq!(rig.agents(&rig.eng).await, []);

    let (status, group) = rig
        .post(
            &rig.admin,
            "/api/groups",
            json!({ "name": "Branch", "description": "branch office", "agent_ids": ["a2", "a3"] }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    assert_eq!(group["agent_ids"], json!(["a2", "a3"]));
    let branch = group["id"].as_i64().unwrap();

    // Remote desktop on a1; scripts on the Branch group.
    let (status, grant) = rig
        .post(
            &rig.admin,
            "/api/grants",
            json!({ "user_id": eng_id, "agent_id": "a1", "capabilities": ["desktop"] }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{grant}");
    let (status, group_grant) = rig
        .post(
            &rig.admin,
            "/api/grants",
            json!({ "user_id": eng_id, "group_id": branch, "capabilities": ["script"] }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(group_grant["group_name"], "Branch");

    assert_eq!(
        rig.agents(&rig.eng).await,
        [
            row("a1", &["desktop"], &[]),
            row("a2", &["script"], &["Branch"]),
            row("a3", &["script"], &["Branch"]),
        ]
    );

    // Remote desktop: a1 yes, a2 no.
    let (status, _) = rig
        .post(&rig.eng, "/api/agents/a1/viewer-sessions", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = rig
        .post(&rig.eng, "/api/agents/a2/viewer-sessions", json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Scripts: the whole group yes (offline, but allowed), a1 no, and a
    // run mixing the two is refused as a whole.
    let (status, report) = rig
        .post(
            &rig.eng,
            "/api/script-runs",
            json!({ "group_ids": [branch], "script": "hostname" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let targets: Vec<&str> = report["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["a2", "a3"]);
    assert_eq!(report["results"][0]["status"], "offline");
    let (status, _) = rig
        .post(
            &rig.eng,
            "/api/script-runs",
            json!({ "agent_ids": ["a1"], "group_ids": [branch], "script": "hostname" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Shells and files: no grant at all.
    let (status, _) = rig.get(&rig.eng, "/api/agents/a2/shell").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = rig.get(&rig.eng, "/api/agents/a1/files?path=C:/x").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Policies: visible agents only; the rest do not exist.
    assert_eq!(
        rig.get(&rig.eng, "/api/agents/a1/policy").await.0,
        StatusCode::OK
    );
    assert_eq!(
        rig.get(&rig.eng, "/api/agents/a4/policy").await.0,
        StatusCode::NOT_FOUND
    );

    let denials = rig.denials().await;
    let summary: Vec<(Value, Value)> = denials
        .iter()
        .map(|d| (d["capability"].clone(), d["agents"].clone()))
        .collect();
    assert_eq!(
        summary,
        [
            (json!("desktop"), json!(["a2"])),
            (json!("script"), json!(["a1"])),
            (json!("shell"), json!(["a2"])),
            (json!("file_transfer"), json!(["a1"])),
        ]
    );

    // Group grants follow membership as it changes.
    let (status, group) = rig
        .post(
            &rig.admin,
            &format!("/api/groups/{branch}/agents"),
            json!({ "agent_ids": ["a4"] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(group["agent_ids"], json!(["a2", "a3", "a4"]));
    assert!(rig
        .agents(&rig.eng)
        .await
        .contains(&row("a4", &["script"], &["Branch"])));

    // Revoking the group grant takes it all away again.
    let id = group_grant["id"].as_i64().unwrap();
    let (status, _) = rig
        .call(
            Method::DELETE,
            &rig.admin,
            &format!("/api/grants/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(rig.agents(&rig.eng).await, [row("a1", &["desktop"], &[])]);

    // An engineer may read their own grants, not anyone else's.
    let (status, own) = rig
        .get(&rig.eng, &format!("/api/grants?user_id={eng_id}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own.as_array().unwrap().len(), 1);
    assert_eq!(
        rig.get(&rig.eng, "/api/grants").await.0,
        StatusCode::FORBIDDEN
    );

    let actions = audit_actions(&rig.db.pool).await;
    for expected in [
        "group.create",
        "group.members",
        "grant.create",
        "grant.delete",
    ] {
        assert!(
            actions.iter().any(|a| a == expected),
            "{expected}: {actions:?}"
        );
    }
}

#[tokio::test]
async fn an_all_agents_grant_covers_agents_enrolled_later() {
    let rig = rig().await;
    let (status, _) = rig
        .post(
            &rig.admin,
            "/api/grants",
            json!({ "user_id": rig.eng_user.user.id, "all_agents": true }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    insert_agent(&rig.db.pool, "a5").await;
    let all = ["desktop", "shell", "script", "file_transfer"];
    let seen = rig.agents(&rig.eng).await;
    assert_eq!(seen.len(), 5);
    assert!(seen.iter().all(|(_, caps, _)| caps == &all));
}

#[tokio::test]
async fn auditors_see_everything_and_do_nothing() {
    let rig = rig().await;
    let seen = rig.agents(&rig.auditor).await;
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|(_, caps, _)| caps.is_empty()));
    let (status, _) = rig
        .post(
            &rig.auditor,
            "/api/script-runs",
            json!({ "agent_ids": ["a1"], "script": "hostname" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Admins do everything, everywhere.
    let seen = rig.agents(&rig.admin).await;
    assert!(seen.iter().all(|(_, caps, _)| caps.len() == 4));
}

#[tokio::test]
async fn only_admins_administer_and_attempts_are_audited() {
    let rig = rig().await;
    let eng_id = rig.eng_user.user.id;
    for (method, path, body) in [
        (Method::GET, "/api/users".to_owned(), None),
        (
            Method::POST,
            "/api/groups".to_owned(),
            Some(json!({ "name": "mine" })),
        ),
        (
            Method::POST,
            "/api/grants".to_owned(),
            Some(json!({ "user_id": eng_id, "all_agents": true })),
        ),
        (
            Method::PUT,
            format!("/api/users/{eng_id}/role"),
            Some(json!({ "role": "admin" })),
        ),
    ] {
        for token in [&rig.eng, &rig.auditor] {
            let (status, _) = rig.call(method.clone(), token, &path, body.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    let actions: Vec<Value> = rig
        .denials()
        .await
        .iter()
        .map(|d| d["action"].clone())
        .collect();
    assert_eq!(
        actions,
        [
            "user.list",
            "user.list",
            "group.create",
            "group.create",
            "grant.create",
            "grant.create",
            "user.role_change",
            "user.role_change"
        ]
    );

    // Grants are only for engineers.
    let (status, body) = rig
        .post(
            &rig.admin,
            "/api/grants",
            json!({ "user_id": 1, "all_agents": true }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = rig
        .post(
            &rig.admin,
            "/api/grants",
            json!({ "user_id": eng_id, "agent_id": "a1", "group_id": 1 }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig
        .post(
            &rig.admin,
            "/api/grants",
            json!({ "user_id": eng_id, "agent_id": "nope" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admins_manage_users_but_never_lose_the_last_admin() {
    let rig = rig().await;
    let (status, created) = rig
        .post(
            &rig.admin,
            "/api/users",
            json!({ "username": "newbie", "password": support::PASSWORD, "role": "support_engineer" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let newbie = created["user"]["id"].as_i64().unwrap();
    // The returned secret works for signing in.
    let secret = created["totp_secret"].as_str().unwrap();
    rig.api.session("newbie", secret).await;
    let (status, _) = rig
        .post(
            &rig.admin,
            "/api/users",
            json!({ "username": "newbie", "password": support::PASSWORD, "role": "auditor" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, user) = rig
        .call(
            Method::PUT,
            &rig.admin,
            &format!("/api/users/{newbie}/role"),
            Some(json!({ "role": "auditor" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(user["role"], "auditor");

    // root is the only admin: it can neither be demoted nor delete itself.
    let (_, users) = rig.get(&rig.admin, "/api/users").await;
    let root = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "root")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, body) = rig
        .call(
            Method::PUT,
            &rig.admin,
            &format!("/api/users/{root}/role"),
            Some(json!({ "role": "auditor" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "that would leave no admin");
    let (status, _) = rig
        .call(
            Method::DELETE,
            &rig.admin,
            &format!("/api/users/{root}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A role change applies to existing sessions at once.
    let (status, _) = rig
        .call(
            Method::PUT,
            &rig.admin,
            &format!("/api/users/{}/role", rig.eng_user.user.id),
            Some(json!({ "role": "admin" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rig.agents(&rig.eng).await.len(), 4);

    let (status, _) = rig
        .call(
            Method::DELETE,
            &rig.admin,
            &format!("/api/users/{newbie}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let actions = audit_actions(&rig.db.pool).await;
    for expected in ["user.create", "user.role_change", "user.delete"] {
        assert!(actions.iter().any(|a| a == expected), "{expected}");
    }
}

#[tokio::test]
async fn groups_are_validated_and_editable() {
    let rig = rig().await;
    let (status, g) = rig
        .post(&rig.admin, "/api/groups", json!({ "name": " Servers " }))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(g["name"], "Servers");
    let id = g["id"].as_i64().unwrap();
    assert_eq!(
        rig.post(&rig.admin, "/api/groups", json!({ "name": "Servers" }))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        rig.post(&rig.admin, "/api/groups", json!({ "name": "" }))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, body) = rig
        .post(
            &rig.admin,
            &format!("/api/groups/{id}/agents"),
            json!({ "agent_ids": ["a1", "ghost"] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "no such agent: ghost");

    let (status, g) = rig
        .call(
            Method::PUT,
            &rig.admin,
            &format!("/api/groups/{id}/agents"),
            Some(json!({ "agent_ids": ["a1", "a3"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(g["agent_ids"], json!(["a1", "a3"]));
    let (status, g) = rig
        .call(
            Method::PATCH,
            &rig.admin,
            &format!("/api/groups/{id}"),
            Some(json!({ "name": "Datacenter" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (g["name"].clone(), g["agent_ids"].clone()),
        (json!("Datacenter"), json!(["a1", "a3"]))
    );
    let (status, g) = rig
        .call(
            Method::DELETE,
            &rig.admin,
            &format!("/api/groups/{id}/agents/a1"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(g["agent_ids"], json!(["a3"]));

    // Engineers can read groups, but only see members they can see.
    let (_, list) = rig.get(&rig.eng, "/api/groups").await;
    assert_eq!(list[0]["name"], "Datacenter");
    assert_eq!(list[0]["agent_ids"], json!([]));

    // Running a script on an empty or unknown group is a bad request.
    let (status, _) = rig
        .post(
            &rig.admin,
            "/api/script-runs",
            json!({ "group_ids": [9999], "script": "x" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = rig
        .call(
            Method::DELETE,
            &rig.admin,
            &format!("/api/groups/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        rig.get(&rig.admin, &format!("/api/groups/{id}")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_grant_revoked_after_the_viewer_token_was_minted_still_stops_the_session() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    let api = start_api_full(
        db.pool.clone(),
        &certs,
        "/nonexistent".into(),
        Some(quic.hub()),
    )
    .await;
    insert_agent(&db.pool, "a1").await;
    let eng = create_user(&db.pool, "eng", Role::SupportEngineer).await;
    let grant = server::access::create_grant(
        &db.pool,
        "root",
        eng.user.id,
        server::access::Scope::Agent("a1".into()),
        &[server::users::Capability::Desktop].into(),
    )
    .await
    .unwrap();
    let session = api.session("eng", &eng.totp_secret).await;
    let token: Value = api
        .client
        .post(api.url("/api/agents/a1/viewer-sessions"))
        .bearer_auth(&session)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    server::access::delete_grant(&db.pool, "root", grant.id)
        .await
        .unwrap();

    let options = viewer::client::ViewerOptions {
        server: addr,
        transport: Default::default(),
        server_name: "localhost".into(),
        ca_pem: certs.ca_cert.clone(),
        token: token["token"].as_str().unwrap().to_owned(),
        bind: Some("127.0.0.1:0".parse().unwrap()),
    };
    let result = tokio::time::timeout(Duration::from_secs(20), viewer::client::connect(&options))
        .await
        .unwrap();
    match result {
        Err(viewer::client::ClientError::Refused(reason)) => {
            assert_eq!(reason, "not allowed to view this agent")
        }
        other => panic!("expected refusal, got {:?}", other.map(|_| ())),
    }
    std::mem::forget(quic);
}

#[tokio::test]
async fn enrollment_links_can_put_new_agents_into_groups_but_only_for_admins() {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    let api = start_api_full(
        db.pool.clone(),
        &certs,
        "/nonexistent".into(),
        Some(quic.hub()),
    )
    .await;
    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let eng = create_user(&db.pool, "eng", Role::SupportEngineer).await;
    let admin = api.session("root", &admin.totp_secret).await;
    let eng = api.session("eng", &eng.totp_secret).await;
    let group = server::groups::create(&db.pool, "root", "New PCs", "")
        .await
        .unwrap();

    let link = |token: String, body: Value| {
        let api = &api;
        async move {
            let resp = api
                .client
                .post(api.url("/api/enrollment-links"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap();
            (resp.status(), resp.json::<Value>().await.unwrap())
        }
    };
    let (status, _) = link(eng.clone(), json!({ "group_ids": [group.id] })).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = link(admin.clone(), json!({ "group_ids": [4242] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = link(admin.clone(), json!({ "group_ids": [group.id] })).await;
    assert_eq!(status, StatusCode::OK);

    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        transport: Default::default(),
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token: body["token"].as_str().unwrap().to_owned(),
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();
    let group = server::groups::get(&db.pool, group.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(group.agent_ids, [credential.agent_id]);
    std::mem::forget(quic);
}
