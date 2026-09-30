//! Screen streaming end to end, on any OS: a real agent session with a
//! synthetic two-monitor source (OpenH264 standing in for DXGI + Media
//! Foundation), the real server relay, and real viewer clients decoding with
//! OpenH264.

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use agent::media::source::{media_channel, MediaCommand};
use reqwest::StatusCode;
use serde_json::Value;
use server::users::Role;
use support::source::{monitors, run_source, SourceLog};
use support::{audit_actions, create_user, start_api_full, start_db, Api};
use tokio::sync::mpsc;
use tokio::time::timeout;
use viewer::client::{self, ViewerEvent, ViewerHandle, ViewerOptions, Welcome};
use viewer::decode::{Picture, VideoDecoder};

const DEADLINE: Duration = Duration::from_secs(20);

struct Rig {
    db: support::TestDb,
    certs: common::devcerts::DevCerts,
    addr: std::net::SocketAddr,
    transport: transport::TransportSettings,
    api: Api,
    agent_id: String,
    log: Arc<SourceLog>,
    admin_session: String,
}

async fn rig() -> Rig {
    rig_on(transport::TransportMode::Auto).await
}

/// A rig whose agent and viewers all use `mode`.
async fn rig_on(mode: transport::TransportMode) -> Rig {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    let transport = transport::TransportSettings {
        mode,
        ws_addr: quic.ws_local_addr(),
    };
    let api = start_api_full(
        db.pool.clone(),
        &certs,
        "/nonexistent".into(),
        Some(quic.hub()),
    )
    .await;
    // Keep the QUIC server alive for the test's duration.
    std::mem::forget(quic);

    let token = server::enroll::create_token(&db.pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        transport,
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();

    let (link, end) = media_channel();
    let log = Arc::new(SourceLog::default());
    run_source(end, log.clone());
    let mut config = credential.agent_config(Duration::from_secs(5)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.media = Some(Arc::new(link));
    let session = agent::connect(&config).await.unwrap();
    tokio::spawn(async move { session.run(None).await });

    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let admin_session = api.session("root", &admin.totp_secret).await;
    // Wait until the agent's monitor list reached the relay.
    let agent_id = credential.agent_id.clone();
    Rig {
        db,
        certs,
        addr,
        transport,
        api,
        agent_id,
        log,
        admin_session,
    }
}

impl Rig {
    async fn viewer_token_as(&self, session: &str) -> reqwest::Response {
        self.api
            .client
            .post(
                self.api
                    .url(&format!("/api/agents/{}/viewer-sessions", self.agent_id)),
            )
            .bearer_auth(session)
            .send()
            .await
            .unwrap()
    }

    async fn viewer_token(&self) -> String {
        let resp = self.viewer_token_as(&self.admin_session).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["online"], true);
        body["token"].as_str().unwrap().to_owned()
    }

    fn options(&self, token: String) -> ViewerOptions {
        ViewerOptions {
            server: self.addr,
            transport: self.transport,
            server_name: "localhost".into(),
            ca_pem: self.certs.ca_cert.clone(),
            token,
            bind: Some("127.0.0.1:0".parse().unwrap()),
            // These tests are about the relay; see direct.rs for direct paths.
            direct: peer::DirectSettings {
                enabled: false,
                ..Default::default()
            },
        }
    }

    async fn viewer(&self) -> Viewer {
        let token = self.viewer_token().await;
        let (welcome, handle, events) = client::connect(&self.options(token)).await.unwrap();
        Viewer {
            welcome,
            handle,
            events,
            decoder: VideoDecoder::new().unwrap(),
            frames: BTreeMap::new(),
        }
    }
}

struct Viewer {
    welcome: Welcome,
    handle: ViewerHandle,
    events: mpsc::Receiver<ViewerEvent>,
    decoder: VideoDecoder,
    /// seq -> raw payload, for comparing what different viewers got.
    frames: BTreeMap<u64, Vec<u8>>,
}

impl Viewer {
    /// Decode until `n` pictures of `monitor` have been shown.
    async fn pictures(&mut self, monitor: u32, n: usize) -> Vec<Picture> {
        let mut out = Vec::new();
        timeout(DEADLINE, async {
            while out.len() < n {
                match self.events.recv().await.expect("viewer events") {
                    ViewerEvent::Frame(frame) => {
                        self.frames.insert(frame.seq, frame.payload.clone());
                        match self.decoder.decode(&frame) {
                            Ok(Some(p)) if p.monitor == monitor => out.push(p),
                            Ok(_) => {}
                            Err(e) => panic!("decode error: {e}"),
                        }
                    }
                    ViewerEvent::Closed(reason) => panic!("viewer closed: {reason}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {n} pictures of monitor {monitor}"));
        out
    }
}

fn assert_colour(p: &Picture, expect: (u8, u8, u8)) {
    // A corner away from the moving square; allow codec rounding.
    let (r, g, b) = p.pixel(p.width - 4, p.height - 4);
    let close = |a: u8, b: u8| a.abs_diff(b) <= 12;
    assert!(
        close(r, expect.0) && close(g, expect.1) && close(b, expect.2),
        "{p:?}: got ({r},{g},{b}), expected about {expect:?}"
    );
}

const RED: (u8, u8, u8) = (255, 0, 0);
const BLUE: (u8, u8, u8) = (0, 0, 255);

#[tokio::test]
async fn streaming_and_monitor_switching_work_over_the_websocket_fallback() {
    // Agent and viewers all on WebSocket over TLS: what a UDP-blocked
    // network gets. Same relay, same fan-out, same frames.
    let rig = rig_on(transport::TransportMode::WebSocket).await;
    let mut a = rig.viewer().await;
    assert_eq!(a.handle.transport(), transport::TransportKind::WebSocket);
    let first = a.pictures(0, 5).await;
    assert_colour(&first[0], RED);

    let mut b = rig.viewer().await;
    b.pictures(0, 3).await;
    a.handle.select_monitor(1);
    let switched = b.pictures(1, 3).await;
    assert_eq!((switched[0].width, switched[0].height), (256, 192));
    assert_colour(&switched[0], BLUE);
    a.pictures(1, 3).await;

    let agents: Value = rig
        .api
        .client
        .get(rig.api.url("/api/agents"))
        .bearer_auth(&rig.admin_session)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(agents[0]["transport"], "websocket");
    assert_eq!(agents[0]["viewer_sessions"], 2);
}

#[tokio::test]
async fn a_viewer_that_falls_behind_makes_the_agent_lower_its_bitrate() {
    // Adaptive bitrate end to end: the viewer acknowledges frames while it
    // keeps up, then stops consuming them (a jammed link looks the same
    // from the server). The relay sees it falling behind and reports it;
    // the agent's controller cuts the encoder's target.
    let rig = rig().await;
    let mut viewer = rig.viewer().await;
    viewer.pictures(0, 10).await;
    assert!(rig.log.bitrates().is_empty(), "{:?}", rig.log.bitrates());

    let full = agent::media::rate::max_bitrate(320, 240);
    let cut = timeout(DEADLINE, async {
        loop {
            if let Some(bps) = rig.log.bitrates().into_iter().find(|b| *b < full) {
                return bps;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the agent should have lowered its bitrate");
    assert!(cut >= agent::media::rate::MIN_BITRATE, "{cut}");
    drop(viewer);
}

#[tokio::test]
async fn two_viewers_are_fed_by_one_encode_via_the_server() {
    let rig = rig().await;

    let mut a = rig.viewer().await;
    assert_eq!(a.welcome.agent_id, rig.agent_id);
    let first = a.pictures(0, 5).await;
    assert_eq!((first[0].width, first[0].height), (320, 240));
    assert_colour(&first[0], RED);

    // A second viewer joins mid-stream: it gets a keyframe, not a new encoder.
    let mut b = rig.viewer().await;
    assert_eq!(b.welcome.monitors, monitors(), "monitor list relayed");
    assert_eq!(b.welcome.active_monitor, Some(0));
    b.pictures(0, 5).await;
    a.pictures(0, 5).await;

    assert_eq!(
        rig.log.starts(),
        1,
        "one Start for two viewers: {:?}",
        rig.log.commands()
    );
    assert!(
        rig.log.commands().contains(&MediaCommand::ForceKeyframe),
        "late joiner asked for a keyframe"
    );
    // Frames with the same sequence number are byte-identical: the same
    // encode, fanned out by the server.
    let common: Vec<u64> = a
        .frames
        .keys()
        .filter(|s| b.frames.contains_key(s))
        .copied()
        .collect();
    assert!(common.len() >= 3, "overlap {common:?}");
    for seq in &common {
        assert_eq!(a.frames[seq], b.frames[seq]);
    }

    // Selecting monitor 1 switches the shared stream for both viewers.
    b.handle.select_monitor(1);
    let pa = a.pictures(1, 3).await;
    let pb = b.pictures(1, 3).await;
    for p in [&pa[0], &pb[0]] {
        assert_eq!((p.width, p.height), (256, 192));
        assert_colour(p, BLUE);
    }
    assert_eq!(rig.log.starts(), 2);

    // When the last viewer leaves, the agent stops capturing.
    a.handle.close();
    b.handle.close();
    drop((a, b));
    timeout(DEADLINE, async {
        while rig.log.commands().last() != Some(&MediaCommand::Stop) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("agent told to stop");

    let actions = audit_actions(&rig.db.pool).await;
    for action in [
        "viewer.session_create",
        "viewer.connect",
        "viewer.disconnect",
    ] {
        assert_eq!(
            actions.iter().filter(|a| *a == action).count(),
            2,
            "{action} in {actions:?}"
        );
    }
}

#[tokio::test]
async fn viewer_tokens_are_single_use_and_role_checked() {
    let rig = rig().await;

    let token = rig.viewer_token().await;
    let (_welcome, handle, _events) = client::connect(&rig.options(token.clone())).await.unwrap();
    match client::connect(&rig.options(token)).await {
        Err(client::ClientError::Refused(reason)) => assert_eq!(reason, "invalid viewer token"),
        other => panic!("reused token accepted: {:?}", other.map(|r| r.0)),
    }
    handle.close();

    match client::connect(&rig.options("made-up".into())).await {
        Err(client::ClientError::Refused(reason)) => assert_eq!(reason, "invalid viewer token"),
        other => panic!("bogus token accepted: {:?}", other.map(|r| r.0)),
    }

    // Auditors are read-only: no screens.
    let auditor = create_user(&rig.db.pool, "carol", Role::Auditor).await;
    let auditor_session = rig.api.session("carol", &auditor.totp_secret).await;
    assert_eq!(
        rig.viewer_token_as(&auditor_session).await.status(),
        StatusCode::FORBIDDEN
    );

    // Unknown agent.
    let resp = rig
        .api
        .client
        .post(rig.api.url("/api/agents/agt-nope/viewer-sessions"))
        .bearer_auth(&rig.admin_session)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn viewer_for_an_offline_agent_is_told_so() {
    let rig = rig().await;
    // An enrolled agent that is not connected.
    support::insert_agent(&rig.db.pool, "agt-offline").await;
    let resp = rig
        .api
        .client
        .post(rig.api.url("/api/agents/agt-offline/viewer-sessions"))
        .bearer_auth(&rig.admin_session)
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["online"], false);
    match client::connect(&rig.options(body["token"].as_str().unwrap().into())).await {
        Err(client::ClientError::Refused(reason)) => assert_eq!(reason, "agent is not connected"),
        other => panic!("expected refusal, got {:?}", other.map(|r| r.0)),
    }
}
