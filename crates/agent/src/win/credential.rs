//! The prompt for a password the user lends the technicians (see
//! `protocol::credential`): a topmost dialog on the user's desktop naming
//! the technician, saying what happens to the password, with one masked
//! field.
//!
//! Like the consent prompt (see `super::consent`) it runs on its own
//! thread, and closes as "no" when nobody answers in time or the request
//! is withdrawn.
//!
//! What is typed leaves this process only as a [`Secret`] for the service,
//! which keeps it (see `super::secret`). The copies made here are wiped;
//! the field's own buffer is emptied before the dialog closes.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use brand::icons;
use protocol::ipc::Secret;
use tracing::{info, warn};

use super::ui::dialog::{self, Block, Button, Dialog, Event, Focus, Lead, Options, Spec};

/// How long the user has to answer.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Longest technician name shown.
const NAME_CHARS: usize = 64;

const LEND: u32 = 1;
const CANCEL: u32 = 2;

/// A prompt on screen. Dropping it does not close it; [`Prompt::cancel`] does.
pub struct Prompt {
    cancel: mpsc::Sender<()>,
}

impl Prompt {
    /// Withdraw the prompt (the technicians left, or the user pressed
    /// Ctrl+F12).
    pub fn cancel(&self) {
        let _ = self.cancel.send(());
    }
}

fn spec(name: &str) -> Spec {
    Spec {
        title: "Remote support \u{b7} Lend a password".into(),
        blocks: vec![
            Block::Header {
                lead: Lead::Icon(icons::PASSWORD),
                title: format!("{name} is asking for a password"),
                subtitle: "To use on this PC while you're away, for example to unlock it.".into(),
            },
            Block::Checks(vec![
                "Stays on this computer".into(),
                format!("Never shown to {name}"),
                "Forgotten when support ends, or when you press Ctrl + F12".into(),
            ]),
            Block::Password {
                label: "Password".into(),
            },
        ],
        buttons: vec![
            Button::new(CANCEL, "Cancel"),
            Button::new(LEND, "Lend password").primary(),
        ],
        focus: Focus::Input,
        default: Some(LEND),
        cancel: Some(CANCEL),
    }
}

/// Ask for a password on behalf of `technician`. `answer` is called once:
/// with what the user typed, or `None` if they declined, did not answer in
/// time, or the prompt was withdrawn.
pub fn show(technician: &str, answer: impl FnOnce(Option<Secret>) + Send + 'static) -> Prompt {
    let (cancel, cancelled) = mpsc::channel();
    let name: String = technician.chars().take(NAME_CHARS).collect();
    std::thread::Builder::new()
        .name("credential-prompt".into())
        .spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            // What the user types next should go into the field.
            let options = Options {
                topmost: true,
                take_keyboard: true,
                ..Default::default()
            };
            let dialog = match Dialog::open(spec(&name), options) {
                Ok(dialog) => dialog,
                Err(e) => {
                    warn!("the password prompt could not be shown: {e}");
                    return answer(None);
                }
            };
            let mut typed = None;
            dialog::run(&dialog, |dialog, events| {
                if cancelled.try_recv() != Err(mpsc::TryRecvError::Empty)
                    || Instant::now() >= deadline
                {
                    return false;
                }
                match events.first() {
                    Some(Event::Button(LEND)) => {
                        typed = dialog.take_password();
                        // Nothing typed: stay open.
                        typed.is_none()
                    }
                    Some(_) => false,
                    None => true,
                }
            });
            // Emptying the field is part of closing.
            drop(dialog);
            info!(typed = typed.is_some(), "password prompt closed");
            answer(typed);
        })
        .expect("spawn credential prompt thread");
    Prompt { cancel }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_starts_in_the_field_and_enter_lends() {
        let spec = spec("roker");
        assert_eq!(spec.focus, Focus::Input);
        assert_eq!((spec.default, spec.cancel), (Some(LEND), Some(CANCEL)));
        assert!(matches!(spec.blocks[2], Block::Password { .. }));
        let Block::Checks(lines) = &spec.blocks[1] else {
            panic!("no assurances")
        };
        assert!(lines.contains(&"Never shown to roker".to_owned()));
    }
}
