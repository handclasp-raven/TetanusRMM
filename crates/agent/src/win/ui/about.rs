//! The About box: which agent this is, who manages the computer, and how
//! to end a remote session.

use std::sync::atomic::{AtomicBool, Ordering};

use brand::icons;

use super::dialog::{self, Block, Button, Dialog, Focus, Lead, Options, Spec};
use super::look::Look;

/// Only one at a time.
static OPEN: AtomicBool = AtomicBool::new(false);

const OK: u32 = 1;

fn spec() -> Spec {
    let look = Look::current();
    let mut blocks = vec![
        Block::Header {
            lead: Lead::Icon(icons::INFO),
            title: look.agent_name(),
            subtitle: format!("Version {}", crate::core::version()),
        },
        Block::Text(format!(
            "This computer is managed remotely by {}. Press Ctrl+F12 at any time to end all \
             remote sessions.",
            look.team()
        )),
    ];
    if look.company.is_some() {
        blocks.push(Block::Label(format!("Powered by {}", brand::PRODUCT)));
    }
    Spec {
        title: format!("About {}", look.agent_name()),
        blocks,
        buttons: vec![Button::new(OK, "OK").primary()],
        focus: Focus::Button(OK),
        default: Some(OK),
        cancel: Some(OK),
    }
}

/// Show the About box, on a thread of its own, unless it is already up.
pub fn show() {
    if OPEN.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        if let Ok(dialog) = Dialog::open(spec(), Options::default()) {
            // Any button, Esc or the close box closes it; the brand may
            // change while it is up.
            dialog::run(&dialog, |dialog, events| {
                dialog.set(spec());
                events.is_empty()
            });
        }
        OPEN.store(false, Ordering::SeqCst);
    });
}
