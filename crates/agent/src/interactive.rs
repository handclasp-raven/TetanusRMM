//! Interactive sessions on the agent: consent, input, clipboard, and the
//! user's Ctrl+F12 kill switch.
//!
//! The server asks for each session with a `SessionRequest`; the agent
//! applies the consent policy ([`protocol::consent::decide`], plus the
//! prompt), and only a session the agent has granted gets input or
//! clipboard. That check is here as well as on the server, so the kill
//! switch takes effect on the agent immediately, whatever the server does.
//!
//! A technician may also ask the user to lend them a password for the
//! length of the session (see [`protocol::credential`]). The desktop keeps
//! it and types it; this side only passes the requests on, for sessions
//! consent has granted, and says when to forget it.
//!
//! [`Sessions`] is the pure bookkeeping; [`DesktopLink`] connects to the
//! thing that owns the user's desktop (on Windows, the session helper via
//! the service's pipe bridge; in tests, a fake).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use protocol::brand::Branding;
use protocol::clipboard::ClipboardData;
use protocol::consent::{ConsentMode, Outcome, PromptAnswer};
use protocol::credential::CredentialEvent;
use protocol::input::{scancode, InputEvent};
use protocol::ipc::Secret;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::input::HeldInput;

/// Extra time allowed on top of the prompt's own timeout for the helper to
/// answer, before the agent gives up and treats the prompt as timed out.
pub const PROMPT_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Active {
    technician: String,
    mode: ConsentMode,
    held: HeldInput,
}

/// Which sessions exist and what each technician is holding down.
#[derive(Debug, Default)]
pub struct Sessions {
    /// Requested, consent not decided yet.
    pending: BTreeSet<u64>,
    active: BTreeMap<u64, Active>,
}

/// What happened when a session left [`Sessions`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ended {
    /// Key-ups and button-ups to inject for what the technician held.
    pub releases: Vec<InputEvent>,
    /// It was waiting on consent: withdraw the prompt.
    pub was_pending: bool,
}

impl Sessions {
    pub fn requested(&mut self, id: u64) {
        self.pending.insert(id);
    }

    /// Consent for `id` was decided. Returns whether the session is now
    /// active: it must still be pending (not ended or killed meanwhile) and
    /// the outcome must allow it.
    pub fn resolved(
        &mut self,
        id: u64,
        technician: &str,
        mode: ConsentMode,
        outcome: Outcome,
    ) -> bool {
        if !self.pending.remove(&id) || !outcome.allows_session() {
            return false;
        }
        self.active.insert(
            id,
            Active {
                technician: technician.to_owned(),
                mode,
                held: HeldInput::default(),
            },
        );
        true
    }

    /// The server ended session `id`.
    pub fn ended(&mut self, id: u64) -> Ended {
        Ended {
            releases: self
                .active
                .remove(&id)
                .map(|mut a| a.held.release_all())
                .unwrap_or_default(),
            was_pending: self.pending.remove(&id),
        }
    }

    pub fn is_active(&self, id: u64) -> bool {
        self.active.contains_key(&id)
    }

    /// Who session `id` is, if consent has granted it.
    pub fn technician(&self, id: u64) -> Option<String> {
        self.active.get(&id).map(|a| a.technician.clone())
    }

    pub fn any_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// The sessions consent has granted, in connection order.
    pub fn active_ids(&self) -> Vec<u64> {
        self.active.keys().copied().collect()
    }

    /// Input from session `id`: `Some` to inject, `None` to drop (not an
    /// active session, or the kill-switch chord, which only the local user
    /// may press).
    pub fn input(&mut self, id: u64, event: InputEvent) -> Option<InputEvent> {
        let ctrl_held = self.active.values().any(|a| a.held.ctrl_held());
        let session = self.active.get_mut(&id)?;
        if ctrl_held
            && event
                == (InputEvent::Key {
                    scancode: scancode::F12,
                    down: true,
                })
        {
            return None;
        }
        session.held.track(&event);
        Some(event)
    }

    /// Ctrl+F12: end everything. Returns the ids of the active sessions
    /// ended (to report), pending ones cancelled, and input to release.
    pub fn terminate_all(&mut self) -> (Vec<u64>, Vec<u64>, Vec<InputEvent>) {
        let pending = std::mem::take(&mut self.pending).into_iter().collect();
        let mut releases = Vec::new();
        let ended = std::mem::take(&mut self.active)
            .into_iter()
            .map(|(id, mut a)| {
                releases.extend(a.held.release_all());
                id
            })
            .collect();
        (ended, pending, releases)
    }

    /// Technicians the user should see in the tray, in connection order.
    /// Unattended sessions are silent, so they are not listed.
    pub fn technicians(&self) -> Vec<String> {
        self.active
            .values()
            .filter(|a| a.mode != ConsentMode::Unattended)
            .map(|a| a.technician.clone())
            .collect()
    }
}

/// Most names the on-screen indicator lists before "+N more".
const INDICATOR_NAMES: usize = 3;

/// Who the on-screen session bar names, for the technicians the user
/// should see (see [`Sessions::technicians`]): their names, and whether
/// there is more than one ("jane **is**", "jane, sam **are**"). `None` to
/// hide the bar. A technician with several viewers open is named once.
pub fn indicator_who(technicians: &[String]) -> Option<(String, bool)> {
    let mut names: Vec<&str> = Vec::new();
    for t in technicians {
        if !names.contains(&t.as_str()) {
            names.push(t);
        }
    }
    if names.is_empty() {
        return None;
    }
    let mut who = names[..names.len().min(INDICATOR_NAMES)].join(", ");
    if names.len() > INDICATOR_NAMES {
        who += &format!(" +{} more", names.len() - INDICATOR_NAMES);
    }
    Some((who, names.len() > 1))
}

/// Commands for whatever owns the user's desktop.
#[derive(Debug)]
pub enum DesktopCommand {
    /// Show the consent prompt; answer on `reply`.
    Prompt {
        request_id: u64,
        technician: String,
        timeout: Duration,
        reply: oneshot::Sender<PromptAnswer>,
    },
    /// Withdraw the prompt for `request_id`.
    CancelPrompt {
        request_id: u64,
    },
    /// "`technician` connected to this machine".
    Toast {
        technician: String,
    },
    /// Everyone connected, for the tray.
    Technicians(Vec<String>),
    Input(InputEvent),
    SetClipboard(ClipboardData),
    /// Ctrl+Alt+Del, which cannot be injected as key events.
    SecureAttention,
    /// Ask the user for a password to lend to the technicians, and keep
    /// it. `reply` gets `Stored`, `Declined` or `Unavailable`; dropping it
    /// counts as declined.
    CredentialPrompt {
        technician: String,
        reply: oneshot::Sender<CredentialEvent>,
    },
    /// Type the lent password where the keyboard focus is. `reply` gets
    /// whether there was one to type.
    CredentialType {
        reply: oneshot::Sender<bool>,
    },
    /// Forget the lent password and withdraw any prompt for one. `reply`
    /// gets whether there was one to forget.
    CredentialForget {
        reply: oneshot::Sender<bool>,
    },
    /// Type text a technician sent from their password manager (see
    /// [`protocol::vault`]) where the keyboard focus is. `reply` gets
    /// whether something was there to take it.
    TypeText {
        text: Secret,
        reply: oneshot::Sender<bool>,
    },
    /// What the user sees from now on wears this company's branding (see
    /// [`protocol::brand`]), or TetanusRMM's own for `None`.
    Branding(Option<Branding>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopEvent {
    /// The user's clipboard changed.
    Clipboard(ClipboardData),
    /// The user pressed Ctrl+F12.
    KillSwitch,
}

/// The agent connection's end of the desktop link.
pub struct DesktopLink {
    pub commands: mpsc::UnboundedSender<DesktopCommand>,
    pub events: Mutex<mpsc::UnboundedReceiver<DesktopEvent>>,
    /// Whether a user is logged on and at an unlocked screen (to be
    /// asked, or told).
    pub user_present: Box<dyn Fn() -> bool + Send + Sync>,
}

/// The desktop's end.
pub struct DesktopEnd {
    pub commands: mpsc::UnboundedReceiver<DesktopCommand>,
    pub events: mpsc::UnboundedSender<DesktopEvent>,
}

pub fn desktop_channel(
    user_present: impl Fn() -> bool + Send + Sync + 'static,
) -> (DesktopLink, DesktopEnd) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    (
        DesktopLink {
            commands: cmd_tx,
            events: Mutex::new(ev_rx),
            user_present: Box::new(user_present),
        },
        DesktopEnd {
            commands: cmd_rx,
            events: ev_tx,
        },
    )
}

impl DesktopLink {
    /// Show the prompt and wait for the answer. A desktop that never
    /// answers counts as a timeout; one that is gone, as unavailable.
    pub async fn prompt(
        &self,
        request_id: u64,
        technician: &str,
        timeout: Duration,
    ) -> PromptAnswer {
        let (reply, answer) = oneshot::channel();
        let sent = self.commands.send(DesktopCommand::Prompt {
            request_id,
            technician: technician.to_owned(),
            timeout,
            reply,
        });
        if sent.is_err() {
            return PromptAnswer::Unavailable;
        }
        match tokio::time::timeout(timeout + PROMPT_GRACE, answer).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => PromptAnswer::Unavailable,
            Err(_) => {
                let _ = self
                    .commands
                    .send(DesktopCommand::CancelPrompt { request_id });
                PromptAnswer::TimedOut
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::input::MouseButton;

    fn key(scancode: u16, down: bool) -> InputEvent {
        InputEvent::Key { scancode, down }
    }

    #[test]
    fn only_granted_sessions_get_input() {
        let mut s = Sessions::default();
        assert_eq!(s.input(1, key(0x1E, true)), None, "never requested");
        s.requested(1);
        assert_eq!(s.input(1, key(0x1E, true)), None, "still pending");
        assert!(!s.resolved(1, "jane", ConsentMode::Require, Outcome::Denied));
        assert_eq!(s.input(1, key(0x1E, true)), None, "denied");

        s.requested(2);
        assert!(s.resolved(2, "jane", ConsentMode::Require, Outcome::Granted));
        assert_eq!(s.input(2, key(0x1E, true)), Some(key(0x1E, true)));
        assert!(s.any_active());
    }

    #[test]
    fn a_session_ended_while_pending_is_never_activated() {
        let mut s = Sessions::default();
        s.requested(1);
        assert_eq!(
            s.ended(1),
            Ended {
                releases: vec![],
                was_pending: true
            }
        );
        // The user accepts the (withdrawn) prompt afterwards: too late.
        assert!(!s.resolved(1, "jane", ConsentMode::Require, Outcome::Granted));
        assert!(!s.any_active());
    }

    #[test]
    fn ending_a_session_releases_what_it_held() {
        let mut s = Sessions::default();
        s.requested(1);
        s.resolved(1, "jane", ConsentMode::Notify, Outcome::Notify);
        s.input(1, key(scancode::LEFT_SHIFT, true));
        s.input(
            1,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
        );
        let ended = s.ended(1);
        assert!(!ended.was_pending);
        assert_eq!(
            ended.releases,
            [
                key(scancode::LEFT_SHIFT, false),
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    down: false
                }
            ]
        );
    }

    #[test]
    fn kill_switch_ends_everything_and_releases_input() {
        let mut s = Sessions::default();
        for (id, mode, outcome) in [
            (1, ConsentMode::Notify, Outcome::Notify),
            (2, ConsentMode::Unattended, Outcome::Unattended),
        ] {
            s.requested(id);
            s.resolved(id, "tech", mode, outcome);
        }
        s.requested(3); // prompt still up
        s.input(2, key(scancode::LEFT_ALT, true));

        let (ended, cancelled, releases) = s.terminate_all();
        assert_eq!(ended, [1, 2]);
        assert_eq!(cancelled, [3]);
        assert_eq!(releases, [key(scancode::LEFT_ALT, false)]);
        assert!(!s.any_active());
        assert_eq!(s.input(1, key(0x1E, true)), None);
        assert!(!s.resolved(3, "tech", ConsentMode::Require, Outcome::Granted));
    }

    #[test]
    fn technicians_cannot_press_the_kill_switch_for_the_user() {
        let mut s = Sessions::default();
        s.requested(1);
        s.resolved(1, "jane", ConsentMode::Notify, Outcome::Notify);
        assert!(
            s.input(1, key(scancode::F12, true)).is_some(),
            "F12 alone is fine"
        );
        s.input(1, key(scancode::F12, false));
        s.input(1, key(scancode::RIGHT_CTRL, true));
        assert_eq!(s.input(1, key(scancode::F12, true)), None);
        // Nor while a second technician holds Ctrl.
        s.requested(2);
        s.resolved(2, "sam", ConsentMode::Notify, Outcome::Notify);
        assert_eq!(s.input(2, key(scancode::F12, true)), None);
        s.input(1, key(scancode::RIGHT_CTRL, false));
        assert!(s.input(2, key(scancode::F12, true)).is_some());
    }

    #[test]
    fn tray_lists_visible_technicians_in_connection_order() {
        let mut s = Sessions::default();
        for (id, name, mode, outcome) in [
            (5, "jane", ConsentMode::Notify, Outcome::Notify),
            (6, "robot", ConsentMode::Unattended, Outcome::Unattended),
            (7, "sam", ConsentMode::Require, Outcome::Granted),
            (8, "jane", ConsentMode::Notify, Outcome::Notify),
        ] {
            s.requested(id);
            s.resolved(id, name, mode, outcome);
        }
        assert_eq!(s.technicians(), ["jane", "sam", "jane"]);
        s.ended(7);
        assert_eq!(s.technicians(), ["jane", "jane"]);
    }

    #[test]
    fn indicator_names_each_technician_once_and_hides_when_nobody_is_there() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(indicator_who(&[]), None);
        assert_eq!(
            indicator_who(&names(&["jane"])),
            Some(("jane".to_owned(), false))
        );
        assert_eq!(
            indicator_who(&names(&["jane", "sam", "jane"])),
            Some(("jane, sam".to_owned(), true))
        );
        assert_eq!(
            indicator_who(&names(&["a", "b", "c", "d", "e", "a"])),
            Some(("a, b, c +2 more".to_owned(), true))
        );
    }

    #[tokio::test]
    async fn prompt_answers_timeouts_and_missing_desktops() {
        let (link, mut end) = desktop_channel(|| true);
        // A desktop that answers.
        let answer = tokio::spawn(async move {
            let answer = link.prompt(1, "jane", Duration::from_secs(30)).await;
            (link, answer)
        });
        match end.commands.recv().await.unwrap() {
            DesktopCommand::Prompt {
                request_id,
                technician,
                timeout,
                reply,
            } => {
                assert_eq!((request_id, technician.as_str()), (1, "jane"));
                assert_eq!(timeout, Duration::from_secs(30));
                reply.send(PromptAnswer::Declined).unwrap();
            }
            other => panic!("{other:?}"),
        }
        let (link, answer) = answer.await.unwrap();
        assert_eq!(answer, PromptAnswer::Declined);

        // A desktop that drops the request: unavailable.
        let dropper = tokio::spawn(async move {
            let cmd = end.commands.recv().await;
            drop(cmd);
            end
        });
        assert_eq!(
            link.prompt(2, "jane", Duration::from_secs(30)).await,
            PromptAnswer::Unavailable
        );
        let end = dropper.await.unwrap();

        // No desktop at all.
        drop(end);
        assert_eq!(
            link.prompt(3, "jane", Duration::from_secs(30)).await,
            PromptAnswer::Unavailable
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_desktop_that_never_answers_times_out_and_the_prompt_is_withdrawn() {
        let (link, mut end) = desktop_channel(|| true);
        let answer = link.prompt(4, "jane", Duration::from_secs(10)).await;
        assert_eq!(answer, PromptAnswer::TimedOut);
        let _prompt = end.commands.recv().await.unwrap();
        assert!(matches!(
            end.commands.recv().await.unwrap(),
            DesktopCommand::CancelPrompt { request_id: 4 }
        ));
    }
}
