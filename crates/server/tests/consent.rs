//! Consent, input, clipboard and the kill switch end to end, on any OS: the
//! real server (with Postgres), a real agent session, and real viewer
//! clients. A fake desktop stands in for the Windows session helper: it
//! records what the agent asks of it and answers consent prompts as told.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::interactive::{desktop_channel, DesktopCommand, DesktopEnd, DesktopEvent};
use protocol::clipboard::ClipboardData;
use protocol::consent::{DeviceKind, PromptAnswer};
use protocol::input::{scancode, InputEvent, MouseButton};
use serde_json::Value;
use server::registry::{self, ConsentMode, DevicePolicy, OnNoUser};
use server::users::{CreatedUser, Role};
use support::{audit_actions, create_user, start_db};
use tokio::sync::mpsc;
use tokio::time::timeout;
use viewer::client::{self, ClientError, Pending, ViewerEvent, ViewerHandle, ViewerOptions};

const DEADLINE: Duration = Duration::from_secs(20);

/// What the agent asked the desktop to do.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Prompt {
        technician: String,
        timeout_secs: u64,
    },
    CancelPrompt(u64),
    Toast(String),
    Technicians(Vec<String>),
    Input(InputEvent),
    SetClipboard(ClipboardData),
}

struct FakeDesktop {
    seen: Mutex<Vec<Seen>>,
    user_present: Arc<AtomicBool>,
    /// How the next prompts are answered.
    answer: Mutex<PromptAnswer>,
    events: mpsc::UnboundedSender<DesktopEvent>,
}

impl FakeDesktop {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn toasts(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter_map(|s| match s {
                Seen::Toast(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    fn prompts(&self) -> usize {
        self.seen()
            .iter()
            .filter(|s| matches!(s, Seen::Prompt { .. }))
            .count()
    }

    /// The tray's current technician list.
    fn tray(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .rev()
            .find_map(|s| match s {
                Seen::Technicians(t) => Some(t),
                _ => None,
            })
            .unwrap_or_default()
    }

    async fn wait_until(&self, what: &str, cond: impl Fn(&[Seen]) -> bool) {
        timeout(DEADLINE, async {
            while !cond(&self.seen()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}; saw {:?}", self.seen()));
    }

    fn press_ctrl_f12(&self) {
        self.events.send(DesktopEvent::KillSwitch).unwrap();
    }
}

fn run_desktop(mut end: DesktopEnd, fake: Arc<FakeDesktop>) {
    tokio::spawn(async move {
        while let Some(command) = end.commands.recv().await {
            let seen = match command {
                DesktopCommand::Prompt {
                    technician,
                    timeout,
                    reply,
                    ..
                } => {
                    let answer = *fake.answer.lock().unwrap();
                    // The user takes a moment to read it.
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = reply.send(answer);
                    });
                    Seen::Prompt {
                        technician,
                        timeout_secs: timeout.as_secs(),
                    }
                }
                DesktopCommand::CancelPrompt { request_id } => Seen::CancelPrompt(request_id),
                DesktopCommand::Toast { technician } => Seen::Toast(technician),
                DesktopCommand::Technicians(list) => Seen::Technicians(list),
                DesktopCommand::Input(event) => Seen::Input(event),
                DesktopCommand::SetClipboard(data) => Seen::SetClipboard(data),
            };
            fake.seen.lock().unwrap().push(seen);
        }
    });
}

struct Rig {
    db: support::TestDb,
    certs: common::devcerts::DevCerts,
    addr: std::net::SocketAddr,
    agent_id: String,
    desktop: Arc<FakeDesktop>,
    admin: CreatedUser,
}

async fn rig_as(kind: DeviceKind) -> Rig {
    let db = start_db().await;
    let certs = common::devcerts::generate("unused").unwrap();
    let (quic, addr, _events) = support::start_quic(db.pool.clone(), &certs);
    std::mem::forget(quic);

    let token = server::enroll::create_token(&db.pool, "root", Duration::from_secs(600))
        .await
        .unwrap()
        .token;
    let credential = agent::enroll::enroll(&agent::enroll::EnrollOptions {
        server_addr: addr,
        server_name: "localhost".into(),
        server_ca_pem: certs.ca_cert.clone(),
        token,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
    })
    .await
    .unwrap();

    let user_present = Arc::new(AtomicBool::new(true));
    let (link, end) = desktop_channel({
        let present = user_present.clone();
        move || present.load(Ordering::SeqCst)
    });
    let desktop = Arc::new(FakeDesktop {
        seen: Mutex::new(Vec::new()),
        user_present,
        answer: Mutex::new(PromptAnswer::Accepted),
        events: end.events.clone(),
    });
    run_desktop(end, desktop.clone());

    let mut config = credential.agent_config(Duration::from_secs(5)).unwrap();
    config.bind_addr = Some("127.0.0.1:0".parse().unwrap());
    config.desktop = Some(Arc::new(link));
    config.device_kind = kind;
    let session = agent::connect(&config).await.unwrap();
    tokio::spawn(async move { session.run(None).await });

    let admin = create_user(&db.pool, "root", Role::Admin).await;
    let rig = Rig {
        db,
        certs,
        addr,
        agent_id: credential.agent_id.clone(),
        desktop,
        admin,
    };
    // The agent's DeviceInfo has been processed once its kind is stored.
    timeout(DEADLINE, async {
        loop {
            let agents = registry::list_agents(&rig.db.pool).await.unwrap();
            if agents.iter().any(|a| a.device_kind.is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("agent reported its device kind");
    rig
}

async fn rig() -> Rig {
    rig_as(DeviceKind::Workstation).await
}

type Connected = (ViewerHandle, mpsc::Receiver<ViewerEvent>);

impl Rig {
    async fn set_policy(&self, mode: ConsentMode, on_no_user: OnNoUser, timeout_secs: i32) {
        registry::update_policy(
            &self.db.pool,
            "root",
            &self.agent_id,
            DevicePolicy {
                consent_mode: mode,
                on_no_user,
                consent_timeout_secs: timeout_secs,
            },
        )
        .await
        .unwrap();
    }

    fn options(&self, token: String) -> ViewerOptions {
        ViewerOptions {
            server: self.addr,
            server_name: "localhost".into(),
            ca_pem: self.certs.ca_cert.clone(),
            token,
            bind: Some("127.0.0.1:0".parse().unwrap()),
        }
    }

    /// Connect a viewer as `user`, recording the pending statuses shown.
    async fn connect_as(
        &self,
        user: &CreatedUser,
    ) -> (Result<Connected, ClientError>, Vec<Pending>) {
        let token = server::viewers::create(&self.db.pool, &user.user, &self.agent_id)
            .await
            .unwrap()
            .token;
        let mut statuses = Vec::new();
        let result = client::connect_with_status(&self.options(token), |p| statuses.push(p))
            .await
            .map(|(welcome, handle, events)| {
                assert_eq!(welcome.agent_id, self.agent_id);
                (handle, events)
            });
        (result, statuses)
    }

    async fn connect(&self) -> Connected {
        self.connect_as(&self.admin)
            .await
            .0
            .expect("session starts")
    }

    async fn refused(&self) -> String {
        match self.connect_as(&self.admin).await.0 {
            Err(ClientError::Refused(reason)) => reason,
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("session started, expected a refusal"),
        }
    }

    /// `(mode, outcome)` of every `session.start` audit row.
    async fn consent_audit(&self) -> Vec<(String, String)> {
        let rows: Vec<Value> = sqlx::query_scalar(
            "SELECT detail FROM audit_log WHERE action = 'session.start' ORDER BY id",
        )
        .fetch_all(&self.db.pool)
        .await
        .unwrap();
        rows.iter()
            .map(|d| {
                (
                    d["mode"].as_str().unwrap().to_owned(),
                    d["outcome"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

fn pair(mode: &str, outcome: &str) -> (String, String) {
    (mode.to_owned(), outcome.to_owned())
}

async fn next_clipboard(events: &mut mpsc::Receiver<ViewerEvent>) -> ClipboardData {
    timeout(DEADLINE, async {
        loop {
            match events.recv().await.expect("viewer events") {
                ViewerEvent::Clipboard(data) => return data,
                ViewerEvent::Closed(reason) => panic!("closed: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .expect("clipboard within deadline")
}

async fn closed_reason(events: &mut mpsc::Receiver<ViewerEvent>) -> String {
    timeout(DEADLINE, async {
        loop {
            match events.recv().await {
                Some(ViewerEvent::Closed(reason)) => return reason,
                Some(_) => {}
                None => return "event stream ended".into(),
            }
        }
    })
    .await
    .expect("viewer closed within deadline")
}

#[tokio::test]
async fn notify_starts_at_once_toasts_and_relays_input_and_clipboard() {
    let rig = rig().await;
    let (viewer, mut events) = rig.connect().await;

    rig.desktop
        .wait_until("toast", |s| s.contains(&Seen::Toast("root".into())))
        .await;
    assert_eq!(rig.desktop.tray(), ["root"]);
    assert_eq!(rig.desktop.prompts(), 0, "notify never prompts");
    assert_eq!(rig.consent_audit().await, [pair("notify", "notify")]);

    // Input: viewer -> server -> agent -> desktop, in order.
    let sent = [
        InputEvent::MouseMove {
            monitor: 0,
            x: 1000,
            y: 2000,
        },
        InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        },
        InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
        },
        InputEvent::Key {
            scancode: scancode::LEFT_SHIFT,
            down: true,
        },
        InputEvent::Key {
            scancode: 0x1E,
            down: true,
        },
        InputEvent::Wheel { dx: 0, dy: -120 },
    ];
    for e in sent {
        viewer.send_input(e);
    }
    rig.desktop
        .wait_until("input", |s| {
            s.iter().filter(|x| matches!(x, Seen::Input(_))).count() == sent.len()
        })
        .await;
    let injected: Vec<InputEvent> = rig
        .desktop
        .seen()
        .into_iter()
        .filter_map(|s| match s {
            Seen::Input(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(injected, sent);

    // Clipboard, both ways.
    viewer.send_clipboard(ClipboardData::Text("from technician".into()));
    rig.desktop
        .wait_until("clipboard set", |s| {
            s.contains(&Seen::SetClipboard(ClipboardData::Text(
                "from technician".into(),
            )))
        })
        .await;
    let files = ClipboardData::Files(vec![r"C:\Users\claude\report.docx".into()]);
    rig.desktop
        .events
        .send(DesktopEvent::Clipboard(files.clone()))
        .unwrap();
    assert_eq!(next_clipboard(&mut events).await, files);

    // Closing the viewer ends the session on the agent: held keys are
    // released and the tray empties.
    viewer.close();
    rig.desktop
        .wait_until("tray emptied", |s| {
            s.last() == Some(&Seen::Technicians(vec![]))
        })
        .await;
    let seen = rig.desktop.seen();
    for released in [
        InputEvent::Key {
            scancode: scancode::LEFT_SHIFT,
            down: false,
        },
        InputEvent::Key {
            scancode: 0x1E,
            down: false,
        },
    ] {
        assert!(
            seen.contains(&Seen::Input(released)),
            "{released:?} released"
        );
    }
}

#[tokio::test]
async fn unattended_is_silent_and_clipboard_stays_private_without_a_session() {
    let rig = rig().await;
    rig.set_policy(ConsentMode::Unattended, OnNoUser::Deny, 30)
        .await;

    // Nobody connected: the user's clipboard goes nowhere.
    rig.desktop
        .events
        .send(DesktopEvent::Clipboard(ClipboardData::Text(
            "secret".into(),
        )))
        .unwrap();

    let (viewer, mut events) = rig.connect().await;
    assert_eq!(
        rig.consent_audit().await,
        [pair("unattended", "unattended")]
    );
    // Prove the session is live, then check nothing was shown.
    viewer.send_input(InputEvent::Wheel { dx: 0, dy: 120 });
    rig.desktop
        .wait_until("input", |s| s.iter().any(|x| matches!(x, Seen::Input(_))))
        .await;
    assert_eq!(rig.desktop.toasts(), Vec::<String>::new());
    assert_eq!(rig.desktop.prompts(), 0);
    assert!(rig.desktop.tray().is_empty(), "unattended is not listed");

    rig.desktop
        .events
        .send(DesktopEvent::Clipboard(ClipboardData::Text("later".into())))
        .unwrap();
    assert_eq!(
        next_clipboard(&mut events).await,
        ClipboardData::Text("later".into()),
        "the earlier 'secret' was never sent"
    );
}

#[tokio::test]
async fn require_blocks_until_the_user_answers() {
    let rig = rig().await;
    rig.set_policy(ConsentMode::Require, OnNoUser::Deny, 45)
        .await;

    // Accept: the viewer is told it is waiting, then the session starts.
    let (result, statuses) = rig.connect_as(&rig.admin).await;
    let (_viewer, _events) = result.expect("accepted session starts");
    assert_eq!(statuses, [Pending::WaitingForConsent { timeout_secs: 45 }]);
    assert!(rig.desktop.seen().contains(&Seen::Prompt {
        technician: "root".into(),
        timeout_secs: 45
    }));
    assert_eq!(rig.desktop.tray(), ["root"], "require sessions are listed");
    assert_eq!(
        rig.desktop.toasts(),
        Vec::<String>::new(),
        "no toast after a prompt"
    );

    // Decline.
    *rig.desktop.answer.lock().unwrap() = PromptAnswer::Declined;
    assert_eq!(rig.refused().await, "the user declined the session");

    // No answer in time (the helper's own timer fired).
    *rig.desktop.answer.lock().unwrap() = PromptAnswer::TimedOut;
    assert_eq!(
        rig.refused().await,
        "the user did not respond to the consent prompt"
    );

    assert_eq!(
        rig.consent_audit().await,
        [
            pair("require", "granted"),
            pair("require", "denied"),
            pair("require", "timeout"),
        ]
    );
    assert_eq!(
        rig.desktop.tray(),
        ["root"],
        "refusals never joined the tray"
    );
}

#[tokio::test]
async fn nobody_logged_on_follows_on_no_user() {
    let rig = rig().await;
    rig.desktop.user_present.store(false, Ordering::SeqCst);

    rig.set_policy(ConsentMode::Require, OnNoUser::Deny, 30)
        .await;
    assert_eq!(
        rig.refused().await,
        "nobody is available to approve the session"
    );

    rig.set_policy(ConsentMode::Require, OnNoUser::Allow, 30)
        .await;
    let _allowed = rig.connect().await;

    rig.set_policy(ConsentMode::Notify, OnNoUser::Deny, 30)
        .await;
    let _notified = rig.connect().await;

    assert_eq!(
        rig.consent_audit().await,
        [
            pair("require", "consent_unavailable"),
            pair("require", "bypassed_no_user"),
            pair("notify", "notify_no_user"),
        ]
    );
    assert_eq!(rig.desktop.prompts(), 0, "nobody to prompt");
    assert_eq!(
        rig.desktop.toasts(),
        Vec::<String>::new(),
        "nobody to toast"
    );
}

#[tokio::test]
async fn concurrent_technicians_each_toast_and_ctrl_f12_drops_them_all() {
    let rig = rig().await;
    let jane = create_user(&rig.db.pool, "jane", Role::SupportEngineer).await;

    let (_root_viewer, mut root_events) = rig.connect().await;
    let (jane_viewer, mut jane_events) = rig.connect_as(&jane).await.0.unwrap();
    rig.desktop
        .wait_until("two toasts", |s| {
            s.iter().filter(|x| matches!(x, Seen::Toast(_))).count() == 2
        })
        .await;
    assert_eq!(rig.desktop.toasts(), ["root", "jane"]);
    assert_eq!(rig.desktop.tray(), ["root", "jane"]);

    // A technician cannot press the kill switch for the user.
    for e in [
        InputEvent::Key {
            scancode: scancode::LEFT_CTRL,
            down: true,
        },
        InputEvent::Key {
            scancode: scancode::F12,
            down: true,
        },
        InputEvent::Key {
            scancode: 0x1E,
            down: true,
        },
    ] {
        jane_viewer.send_input(e);
    }
    rig.desktop
        .wait_until("A key", |s| {
            s.contains(&Seen::Input(InputEvent::Key {
                scancode: 0x1E,
                down: true,
            }))
        })
        .await;
    assert!(!rig.desktop.seen().contains(&Seen::Input(InputEvent::Key {
        scancode: scancode::F12,
        down: true
    })));

    rig.desktop.press_ctrl_f12();
    let reason = "the user ended the session (Ctrl+F12)";
    assert_eq!(closed_reason(&mut root_events).await, reason);
    assert_eq!(closed_reason(&mut jane_events).await, reason);
    assert!(rig.desktop.tray().is_empty());
    // Jane's held Ctrl was released.
    assert!(rig.desktop.seen().contains(&Seen::Input(InputEvent::Key {
        scancode: scancode::LEFT_CTRL,
        down: false
    })));

    let actions = audit_actions(&rig.db.pool).await;
    assert_eq!(
        actions
            .iter()
            .filter(|a| *a == "session.user_terminated")
            .count(),
        2,
        "{actions:?}"
    );
    // A new session needs consent again, and works.
    let _again = rig.connect().await;
}

#[tokio::test]
async fn device_kind_picks_the_default_until_an_admin_decides() {
    let rig = rig_as(DeviceKind::Server).await;
    let policy = registry::get_policy(&rig.db.pool, &rig.agent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(policy.consent_mode, ConsentMode::Unattended);
    let actions = audit_actions(&rig.db.pool).await;
    assert!(
        actions.contains(&"policy.default".to_owned()),
        "{actions:?}"
    );

    // Workstation default is notify; an admin's choice sticks.
    registry::record_device_kind(&rig.db.pool, &rig.agent_id, DeviceKind::Workstation)
        .await
        .unwrap();
    let policy = registry::get_policy(&rig.db.pool, &rig.agent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(policy.consent_mode, ConsentMode::Notify);
    rig.set_policy(ConsentMode::Require, OnNoUser::Allow, 30)
        .await;
    assert_eq!(
        registry::record_device_kind(&rig.db.pool, &rig.agent_id, DeviceKind::Server)
            .await
            .unwrap(),
        None
    );
    let policy = registry::get_policy(&rig.db.pool, &rig.agent_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(policy.consent_mode, ConsentMode::Require);
}
