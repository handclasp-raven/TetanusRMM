//! The blocking consent prompt (`require` mode): a topmost dialog on the
//! user's desktop naming the technician, what allowing them means, and a
//! bar that runs down to the moment it declines by itself. It runs on its
//! own thread so the tray, the Ctrl+F12 hotkey and other prompts keep
//! working. It closes as "no" when the timeout passes (answer: timed out)
//! or when the server withdraws the request (no answer is sent).
//!
//! Allow is the dialog's main button, but the keyboard starts on Decline:
//! Enter, or a stray key meant for another window, never lets anyone in.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use brand::icons;
use protocol::consent::PromptAnswer;
use tracing::{info, warn};

use super::ui::dialog::{
    self, Block, Button, Dialog, Event, Focus, Lead, Options, Row, Spec, Tone,
};
use super::ui::look::Look;

const ALLOW: u32 = 1;
const DECLINE: u32 = 2;

/// Longest technician name shown.
const NAME_CHARS: usize = 64;

/// A prompt on screen. Dropping it does not close it; [`Prompt::cancel`] does.
pub struct Prompt {
    cancel: mpsc::Sender<()>,
}

impl Prompt {
    /// Withdraw the prompt (the technician left, or the user pressed Ctrl+F12).
    pub fn cancel(&self) {
        let _ = self.cancel.send(());
    }
}

/// A technician's initial, for their avatar.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

fn spec(name: &str, remaining: Duration, timeout: Duration) -> Spec {
    let look = Look::current();
    // Whole seconds left, counting the one in progress.
    let secs = remaining.as_millis().div_ceil(1000);
    Spec {
        title: "Remote support request".into(),
        blocks: vec![
            Block::Header {
                lead: Lead::Avatar(initial(name)),
                title: format!("{name} wants to control this PC"),
                subtitle: format!("From {}", look.team()),
            },
            Block::Label(format!("If you allow it, {name} can:")),
            Block::Rows(vec![
                Row {
                    icon: icons::VIEW,
                    tone: Tone::Accent,
                    text: "See everything on your screen".into(),
                },
                Row {
                    icon: icons::REMOTE,
                    tone: Tone::Accent,
                    text: "Use your mouse and keyboard".into(),
                },
            ]),
            Block::Countdown {
                fraction: remaining.as_secs_f32() / timeout.as_secs_f32().max(1.0),
                left: format!("Declines automatically in {secs} s"),
            },
        ],
        buttons: vec![
            Button::new(DECLINE, "Decline"),
            Button::new(ALLOW, "Allow").primary(),
        ],
        focus: Focus::Button(DECLINE),
        default: None,
        cancel: Some(DECLINE),
    }
}

/// Show the prompt for `technician`. `answer` is called once with the
/// user's answer, unless the prompt is cancelled.
pub fn show(
    technician: &str,
    timeout: Duration,
    answer: impl FnOnce(PromptAnswer) + Send + 'static,
) -> Prompt {
    let (cancel, cancelled) = mpsc::channel();
    let name: String = technician.chars().take(NAME_CHARS).collect();
    std::thread::Builder::new()
        .name("consent-prompt".into())
        .spawn(move || {
            let deadline = Instant::now() + timeout;
            let options = Options {
                topmost: true,
                ..Default::default()
            };
            let dialog = match Dialog::open(spec(&name, timeout, timeout), options) {
                Ok(dialog) => dialog,
                Err(e) => {
                    // Nothing was shown, so nobody could say yes.
                    warn!("the consent prompt could not be shown: {e}");
                    return answer(PromptAnswer::Unavailable);
                }
            };
            let mut reply = None;
            dialog::run(&dialog, |dialog, events| {
                if cancelled.try_recv() != Err(mpsc::TryRecvError::Empty) {
                    // Withdrawn (or its owner is gone): no answer.
                    return false;
                }
                if let Some(event) = events.first() {
                    reply = Some(match event {
                        Event::Button(ALLOW) => PromptAnswer::Accepted,
                        _ => PromptAnswer::Declined,
                    });
                    return false;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    reply = Some(PromptAnswer::TimedOut);
                    return false;
                }
                dialog.set(spec(&name, remaining, timeout));
                true
            });
            drop(dialog);
            info!(?reply, "consent prompt closed");
            if let Some(reply) = reply {
                answer(reply);
            }
        })
        .expect("spawn consent prompt thread");
    Prompt { cancel }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatars_show_the_first_letter() {
        assert_eq!(initial("roker"), "R");
        assert_eq!(initial(" jane doe"), "J");
        assert_eq!(initial("9lives"), "9");
        assert_eq!(initial("--"), "?");
        assert_eq!(initial(""), "?");
    }

    #[test]
    fn the_keyboard_starts_on_decline() {
        let spec = spec(
            "roker",
            Duration::from_millis(47_200),
            Duration::from_secs(60),
        );
        assert_eq!(spec.focus, Focus::Button(DECLINE));
        assert_eq!((spec.default, spec.cancel), (None, Some(DECLINE)));
        let Block::Countdown { fraction, left } = &spec.blocks[3] else {
            panic!("no countdown")
        };
        assert_eq!(left, "Declines automatically in 48 s");
        assert!((fraction - 0.787).abs() < 0.01);
    }
}
