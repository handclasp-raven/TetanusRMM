//! The viewer's side of clipboard sync: poll the local clipboard (there is
//! no portable change notification), send changes, and write what the
//! remote user copies. [`protocol::clipboard::ClipboardGuard`] stops the two
//! sides bouncing the same content back and forth.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use protocol::clipboard::{ClipboardData, ClipboardGuard};
use tracing::{debug, info, warn};

use crate::client::ViewerHandle;

/// How often the local clipboard is checked for changes.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Run clipboard sync on the current thread until `incoming` closes.
/// `incoming` carries the remote user's clipboard.
pub fn run(handle: ViewerHandle, incoming: Receiver<ClipboardData>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            warn!("clipboard sync disabled: {e}");
            // Keep draining so the sender never blocks on us.
            while incoming.recv().is_ok() {}
            return;
        }
    };
    let mut guard = ClipboardGuard::default();
    // Whatever was copied before connecting stays private.
    guard.prime(read(&mut clipboard));
    loop {
        match incoming.recv_timeout(POLL_INTERVAL) {
            Ok(data) => {
                if guard.remote(&data) {
                    // A file list arrives as its paths; we hold it as text.
                    let text = data.to_text();
                    match clipboard.set_text(text.clone()) {
                        Ok(()) => info!(bytes = text.len(), "remote clipboard applied"),
                        Err(e) => warn!("setting clipboard: {e}"),
                    }
                    // What our next poll will read back is not new.
                    guard.prime(Some(ClipboardData::Text(text)));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(data) = read(&mut clipboard) {
            if let Some(data) = guard.local_changed(data) {
                debug!(bytes = data.size(), "sending clipboard");
                handle.send_clipboard(data);
            }
        }
    }
}

fn read(clipboard: &mut arboard::Clipboard) -> Option<ClipboardData> {
    // Non-text content (images) or an empty clipboard: nothing to sync.
    clipboard.get_text().ok().map(ClipboardData::Text)
}
