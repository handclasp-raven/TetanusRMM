//! End-to-end encryption and direct paths, end to end on one machine: the
//! real server (relay, STUN responder, Postgres), a real agent session with
//! a synthetic screen and a fake desktop, and real viewer clients.
//!
//! On loopback there is no NAT, so these tests exercise everything except
//! the NAT itself: the sealed session over the relay, signaling, candidate
//! gathering (host and STUN), the direct QUIC connection, the seamless
//! switch, falling back, and failing over to nothing. `nat.rs` runs the same
//! system behind two real (network-namespace) NATs.
//!
//! Metrics are process-wide, so the tests here run one at a time.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::interactive::{desktop_channel, DesktopCommand, DesktopEvent};
use agent::media::source::{media_channel, MediaCommand};
use peer::DirectSettings;
use protocol::clipboard::ClipboardData;
use protocol::e2e::{Envelope, Path};
use protocol::input::InputEvent;
use protocol::media::MediaFrame;
use server::relay::{Hub, Observed};
use server::users::{CreatedUser, Role};
use support::source::{run_source, SourceLog};
use support::{create_user, start_db};
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use viewer::client::{self, ViewerEvent, ViewerHandle, ViewerOptions};
use viewer::decode::VideoDecoder;

const DEADLINE: Duration = Duration::from_secs(20);

/// Metrics are shared by every test in this binary.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the agent asked of the (fake) user's desktop.
#[derive(Default)]
struct Desktop {
    input: Mutex<Vec<InputEvent>>,
    clipboard: Mutex<Vec<ClipboardData>>,
}

struct Rig {
    db: support::TestDb,
    certs: common::devcerts::DevCerts,
    addr: std::net::SocketAddr,
    hub: Arc<Hub>,
    agent_id: String,
    log: Arc<SourceLog>,
    desktop: Arc<Desktop>,
    /// Makes the user's clipboard change.
    desktop_events: mpsc::UnboundedSender<DesktopEvent>,
    admin: CreatedUser,
}

/// The agent's direct-path settings for a test.
fn agent_direct(advertise: Option<Vec<std::net::SocketAddr>>) -> DirectSettings {
    DirectSettings {
        bind: Some("127.0.0.1".parse().unwrap()),
        advertise,
        timeout: Duration::from_secs(3),
        ..Default::default()
    }
}

/// A viewer that stays on the relay for `delay` before trying direct.
fn viewer_direct(delay: Duration) -> DirectSettings {
    DirectSettings {
        bind: Some("127.0.0.1".parse().unwrap()),
        delay,
        timeout: Duration::from_secs(3),
        ..Default::default()
    }
}

fn relay_only() -> DirectSettings {
    DirectSettings {
        enabled: false,
        ..Default::default()
    }
}

async fn rig(direct: DirectSettings) -> Rig {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic_direct(db.pool.clone(), &certs).await;
    let hub = quic.hub();
    std::mem::forget(quic);

    let token = server::enroll::create_token(&db.pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        transport: Default::default(),
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();

    let (media, source) = media_channel();
    let log = Arc::new(SourceLog::default());
    run_source(source, log.clone());

    let (link, mut end) = desktop_channel(|| false);
    let desktop = Arc::new(Desktop::default());
    let desktop_events = end.events.clone();
    tokio::spawn({
        let desktop = desktop.clone();
        async move {
            while let Some(command) = end.commands.recv().await {
                match command {
                    DesktopCommand::Input(event) => desktop.input.lock().unwrap().push(event),
                    DesktopCommand::SetClipboard(data) => {
                        desktop.clipboard.lock().unwrap().push(data)
                    }
                    _ => {}
                }
            }
        }
    });

    let mut config = credential.agent_config(Duration::from_secs(5)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.media = Some(Arc::new(media));
    config.desktop = Some(Arc::new(link));
    config.direct = direct;
    let session = agent::connect(&config).await.unwrap();
    tokio::spawn(async move { session.run(None).await });

    let admin = create_user(&db.pool, "root", Role::Admin).await;
    Rig {
        db,
        certs,
        addr,
        hub,
        agent_id: credential.agent_id,
        log,
        desktop,
        desktop_events,
        admin,
    }
}

impl Rig {
    async fn viewer(&self, direct: DirectSettings) -> Viewer {
        let token = server::viewers::create(&self.db.pool, &self.admin.user, &self.agent_id)
            .await
            .unwrap()
            .token;
        let options = ViewerOptions {
            server: self.addr,
            transport: Default::default(),
            server_name: "localhost".into(),
            ca_pem: self.certs.ca_cert.clone(),
            token,
            bind: Some("127.0.0.1:0".parse().unwrap()),
            direct,
        };
        let (_welcome, handle, events) = client::connect(&options).await.unwrap();
        Viewer {
            handle,
            events,
            decoder: VideoDecoder::new().unwrap(),
            seqs: Vec::new(),
            plain: BTreeMap::new(),
            paths: Vec::new(),
            clipboard: Vec::new(),
        }
    }

    /// `session.path` audit entries: the paths recorded, in order.
    async fn audited_paths(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT detail->>'path' FROM audit_log WHERE action = 'session.path' ORDER BY id",
        )
        .fetch_all(&self.db.pool)
        .await
        .unwrap()
    }

    fn force_keyframes(&self) -> usize {
        self.log
            .commands()
            .iter()
            .filter(|c| **c == MediaCommand::ForceKeyframe)
            .count()
    }

    async fn wait_for_desktop(&self, what: &str, cond: impl Fn(&Desktop) -> bool) {
        timeout(DEADLINE, async {
            while !cond(&self.desktop) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the desktop never got {what}"));
    }
}

struct Viewer {
    handle: ViewerHandle,
    events: mpsc::Receiver<ViewerEvent>,
    decoder: VideoDecoder,
    /// Sequence numbers of the frames delivered, in delivery order.
    seqs: Vec<u64>,
    /// Decrypted payloads by sequence number.
    plain: BTreeMap<u64, Vec<u8>>,
    /// Path changes reported, in order.
    paths: Vec<Path>,
    clipboard: Vec<ClipboardData>,
}

impl Viewer {
    /// Decode `n` more pictures. Any decode error fails the test: every
    /// path change here must be seamless.
    async fn pictures(&mut self, n: usize) {
        self.pictures_until(n, |_| true).await;
    }

    /// Decode at least `n` more pictures and until `done` holds.
    async fn pictures_until(&mut self, n: usize, done: impl Fn(&Viewer) -> bool) {
        let mut decoded = 0;
        timeout(DEADLINE, async {
            while decoded < n || !done(self) {
                match self.events.recv().await.expect("viewer events") {
                    ViewerEvent::Frame(frame) => {
                        self.seqs.push(frame.seq);
                        self.plain.insert(frame.seq, frame.payload.clone());
                        match self.decoder.decode(&frame) {
                            Ok(Some(_)) => decoded += 1,
                            Ok(None) => {}
                            Err(e) => panic!("decode error at seq {}: {e}", frame.seq),
                        }
                    }
                    ViewerEvent::Path(path) => self.paths.push(path),
                    ViewerEvent::Clipboard(data) => self.clipboard.push(data),
                    ViewerEvent::Closed(reason) => panic!("viewer closed: {reason}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out after {decoded} of {n} pictures (paths {:?}, now {:?})",
                self.paths,
                self.handle.path()
            )
        });
    }

    /// Every frame since the first one was delivered once, in order.
    fn assert_contiguous(&self) {
        for pair in self.seqs.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "gap or reorder in {:?}", self.seqs);
        }
    }
}

/// Read a metric's current value from the server's registry.
fn metric(name: &str, labels: &str) -> f64 {
    let text = server::metrics::get().render();
    let series = if labels.is_empty() {
        format!("{name} ")
    } else {
        format!("{name}{{{labels}}} ")
    };
    text.lines()
        .find_map(|line| line.strip_prefix(&series))
        .map_or(0.0, |v| v.trim().parse().unwrap())
}

fn viewers_on(path: &str) -> f64 {
    metric("rmm_viewers_by_path", &format!("path=\"{path}\""))
}

fn path_changes(path: &str) -> f64 {
    metric(
        "rmm_session_path_changes_total",
        &format!("path=\"{path}\""),
    )
}

fn relay_frames_in() -> f64 {
    metric("rmm_relay_frames_received_total", "")
}

/// Handshake messages the relay carried so far.
fn handshakes(observed: &mut broadcast::Receiver<Observed>) -> usize {
    let mut n = 0;
    while let Ok(o) = observed.try_recv() {
        if let Observed::Sealed(data) = o {
            if matches!(postcard::from_bytes(&data), Ok(Envelope::Handshake(_))) {
                n += 1;
            }
        }
    }
    n
}

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    timeout(DEADLINE, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn a_session_starts_on_the_relay_and_moves_to_a_direct_path_seamlessly() {
    let _serial = SERIAL.lock().await;
    let rig = rig(agent_direct(None)).await;
    let mut observed = rig.hub.observe();
    let (direct_before, stun_before) =
        (viewers_on("direct"), metric("rmm_stun_requests_total", ""));

    // Instant start on the relay.
    let mut v = rig.viewer(viewer_direct(Duration::from_secs(2))).await;
    let fingerprint = v.handle.session_fingerprint();
    v.pictures(10).await;
    assert_eq!(v.handle.path(), Path::Relayed);
    assert_eq!(viewers_on("direct"), direct_before);

    // Then the direct path, while pictures keep coming.
    v.pictures_until(10, |v| v.paths.contains(&Path::Direct))
        .await;
    v.pictures(30).await;
    assert_eq!(v.handle.path(), Path::Direct);
    assert_eq!(v.paths, [Path::Direct]);
    assert!(
        metric("rmm_stun_requests_total", "") > stun_before,
        "candidates came from STUN"
    );

    // Seamless: no frame lost, repeated or reordered across the switch (the
    // viewer merged both paths), nothing failed to decode, and no keyframe
    // was needed for it.
    v.assert_contiguous();
    assert_eq!(rig.force_keyframes(), 0, "{:?}", rig.log.commands());

    // No re-key: the same session, and the relay carried exactly one
    // handshake (three messages).
    assert_eq!(v.handle.session_fingerprint(), fingerprint);
    assert_eq!(handshakes(&mut observed), 3);

    // Confirmed by metrics and the audit log.
    assert_eq!(viewers_on("direct"), direct_before + 1.0);
    assert_eq!(rig.audited_paths().await, ["direct"]);

    // The video has left the server: the agent stopped sending it there,
    // while the viewer keeps receiving.
    let relayed = relay_frames_in();
    v.pictures(20).await;
    assert_eq!(
        relay_frames_in(),
        relayed,
        "frames still going through the relay"
    );

    // Input and clipboard take the direct path too, and still arrive.
    v.handle.send_input(InputEvent::MouseMove {
        x: 1,
        y: 2,
        monitor: 0,
    });
    v.handle
        .send_clipboard(ClipboardData::Text("over the direct path".into()));
    rig.wait_for_desktop("input and clipboard", |d| {
        !d.input.lock().unwrap().is_empty() && !d.clipboard.lock().unwrap().is_empty()
    })
    .await;
    rig.desktop_events
        .send(DesktopEvent::Clipboard(ClipboardData::Text(
            "from the user".into(),
        )))
        .unwrap();
    v.pictures_until(1, |v| !v.clipboard.is_empty()).await;
    assert_eq!(v.clipboard, [ClipboardData::Text("from the user".into())]);

    v.handle.close();
    drop(v);
    eventually("the viewer to be counted out", || {
        viewers_on("direct") == direct_before
    })
    .await;
    let disconnect: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'viewer.disconnect' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&rig.db.pool)
    .await
    .unwrap();
    assert_eq!(disconnect["path"], "direct");
}

#[tokio::test]
async fn the_relay_sees_only_ciphertext() {
    let _serial = SERIAL.lock().await;
    let rig = rig(agent_direct(None)).await;
    let mut observed = rig.hub.observe();

    // Everything through the relay, so the relay handles all of it.
    let mut v = rig.viewer(relay_only()).await;
    v.pictures(15).await;
    let secret_in = "viewer secret: hunter2";
    let secret_out = "user secret: correct horse";
    v.handle
        .send_clipboard(ClipboardData::Text(secret_in.into()));
    let typed = InputEvent::Key {
        scancode: 0x1E,
        down: true,
    };
    v.handle.send_input(typed);
    rig.wait_for_desktop("the viewer's clipboard", |d| {
        d.clipboard
            .lock()
            .unwrap()
            .contains(&ClipboardData::Text(secret_in.into()))
    })
    .await;
    assert_eq!(*rig.desktop.input.lock().unwrap(), [typed]);
    rig.desktop_events
        .send(DesktopEvent::Clipboard(ClipboardData::Text(
            secret_out.into(),
        )))
        .unwrap();
    v.pictures_until(5, |v| !v.clipboard.is_empty()).await;
    assert_eq!(v.clipboard, [ClipboardData::Text(secret_out.into())]);

    let mut frames: BTreeMap<u64, Arc<MediaFrame>> = BTreeMap::new();
    let mut sealed = Vec::new();
    while let Ok(o) = observed.try_recv() {
        match o {
            Observed::Frame(f) => {
                frames.insert(f.seq, f);
            }
            Observed::Sealed(data) => sealed.push(data),
        }
    }

    // Video: every frame the viewer decoded went through the relay sealed.
    let mut compared = 0;
    for (seq, plain) in &v.plain {
        let relayed = &frames[seq];
        compared += 1;
        assert_ne!(&relayed.payload, plain);
        let video = postcard::from_bytes::<protocol::media::VideoPayload>(plain).unwrap();
        assert_ne!(relayed.video().ok(), Some(video.clone()));
        // No stretch of the H.264 bitstream shows through.
        for chunk in video.h264.chunks(16).filter(|c| c.len() == 16) {
            assert!(
                !relayed.payload.windows(16).any(|w| w == chunk),
                "frame {seq}: plaintext H.264 visible to the relay"
            );
        }
    }
    assert!(compared >= 20, "compared {compared} frames");

    // Records: the relay carried the handshake and sealed records, and
    // none of the secrets, the keystroke, or any plaintext record.
    assert!(sealed.len() >= 3 + 3, "{} sealed messages", sealed.len());
    let keystroke = postcard::to_stdvec(&protocol::e2e::Control::Input(typed)).unwrap();
    for data in &sealed {
        for needle in [secret_in.as_bytes(), secret_out.as_bytes(), b"secret"] {
            assert!(!data.windows(needle.len()).any(|w| w == needle));
        }
        assert!(!data
            .windows(keystroke.len())
            .any(|w| w == keystroke.as_slice()));
        // Only handshakes and sealed records: nothing else crosses.
        assert!(postcard::from_bytes::<Envelope>(data).is_ok());
    }
}

#[tokio::test]
async fn when_punching_fails_the_session_stays_on_the_relay_and_works() {
    let _serial = SERIAL.lock().await;
    // The agent advertises an address where nothing answers: every
    // connection attempt goes unanswered, as behind a symmetric NAT.
    let black_hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let rig = rig(agent_direct(Some(vec![black_hole.local_addr().unwrap()]))).await;
    let failed_before = path_changes("direct_failed");
    let direct_before = viewers_on("direct");

    let mut v = rig.viewer(viewer_direct(Duration::ZERO)).await;
    v.pictures(5).await;
    // The attempt fails (connect timeout) while video carries on.
    let audited = timeout(DEADLINE, async {
        loop {
            v.pictures(1).await;
            let paths = rig.audited_paths().await;
            if !paths.is_empty() {
                return paths;
            }
        }
    })
    .await
    .expect("the failed attempt was reported");
    assert_eq!(audited, ["direct_failed"]);
    assert_eq!(path_changes("direct_failed"), failed_before + 1.0);
    assert_eq!(viewers_on("direct"), direct_before);

    // Still on the relay, and everything still works.
    v.pictures(30).await;
    assert_eq!(v.handle.path(), Path::Relayed);
    assert_eq!(v.paths, [Path::DirectFailed]);
    v.assert_contiguous();
    v.handle
        .send_clipboard(ClipboardData::Text("via the relay".into()));
    rig.wait_for_desktop("the clipboard", |d| !d.clipboard.lock().unwrap().is_empty())
        .await;
}

#[tokio::test]
async fn a_dropped_direct_path_falls_back_to_the_relay() {
    let _serial = SERIAL.lock().await;
    let rig = rig(agent_direct(None)).await;
    let mut observed = rig.hub.observe();
    let direct_before = viewers_on("direct");

    let mut v = rig.viewer(viewer_direct(Duration::ZERO)).await;
    let fingerprint = v.handle.session_fingerprint();
    v.pictures_until(5, |v| v.paths.contains(&Path::Direct))
        .await;
    v.pictures(10).await;
    let relayed = relay_frames_in();

    // The direct path dies.
    v.handle.drop_direct_path();
    v.pictures_until(20, |v| v.paths.last() == Some(&Path::Relayed))
        .await;
    assert_eq!(v.paths, [Path::Direct, Path::Relayed]);
    assert_eq!(v.handle.path(), Path::Relayed);
    // Video comes through the relay again...
    assert!(relay_frames_in() > relayed);
    assert_eq!(viewers_on("direct"), direct_before);
    // ...in the same session: no new handshake.
    assert_eq!(v.handle.session_fingerprint(), fingerprint);
    assert_eq!(handshakes(&mut observed), 3);
    assert_eq!(rig.audited_paths().await, ["direct", "relayed"]);

    // Input still reaches the desktop over the relay.
    v.handle.send_input(InputEvent::MouseMove {
        x: 5,
        y: 5,
        monitor: 0,
    });
    rig.wait_for_desktop("input", |d| !d.input.lock().unwrap().is_empty())
        .await;
}

#[tokio::test]
async fn direct_paths_can_be_turned_off_by_the_server() {
    let _serial = SERIAL.lock().await;
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let quic = server::Server::bind(server::ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        ws_listen: None,
        identity: certs.server_identity().unwrap(),
        client_ca: certs.ca().unwrap(),
    })
    .unwrap()
    .with_events(tx)
    .with_registry(server::Registry {
        pool: db.pool.clone(),
        ca: server::enroll::AgentCa::from_pem(&certs.ca_cert, &certs.ca_key).unwrap(),
        api_url: "https://localhost:8443".into(),
    })
    .with_direct_paths(false, None);
    let addr = quic.local_addr().unwrap();
    let quic = Arc::new(quic);
    tokio::spawn({
        let quic = quic.clone();
        async move { quic.run().await }
    });
    let token = server::enroll::create_token(&db.pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        transport: Default::default(),
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();
    let (media, source) = media_channel();
    run_source(source, Arc::new(SourceLog::default()));
    let mut config = credential.agent_config(Duration::from_secs(5)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.media = Some(Arc::new(media));
    config.direct = agent_direct(None);
    let session = agent::connect(&config).await.unwrap();
    tokio::spawn(async move { session.run(None).await });
    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let token = server::viewers::create(&db.pool, &admin.user, &credential.agent_id)
        .await
        .unwrap()
        .token;
    let (_welcome, handle, mut events) = client::connect(&ViewerOptions {
        server: addr,
        transport: Default::default(),
        server_name: "localhost".into(),
        ca_pem: certs.ca_cert.clone(),
        token,
        bind: Some("127.0.0.1:0".parse().unwrap()),
        direct: viewer_direct(Duration::ZERO),
    })
    .await
    .unwrap();
    // Well past when a direct path would have come up on loopback.
    let mut frames = 0;
    timeout(Duration::from_secs(3), async {
        while let Some(event) = events.recv().await {
            match event {
                ViewerEvent::Frame(_) => frames += 1,
                ViewerEvent::Path(path) => panic!("path changed to {path}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap_err();
    assert!(frames > 10);
    assert_eq!(handle.path(), Path::Relayed);
    let paths: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action = 'session.path'")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert!(paths.is_empty(), "no attempt was even made: {paths:?}");
}
