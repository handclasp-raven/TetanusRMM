//! Two-way clipboard sync between viewer and agent.
//!
//! Each end watches its local clipboard and sends changes; the other end
//! writes what it receives into its own clipboard. Writing the clipboard
//! makes the OS report a change, which would be sent straight back, and with
//! two ends doing that the same text would bounce forever. [`ClipboardGuard`]
//! breaks the loop: an end only sends content that differs from the last
//! content it saw from either side.
//!
//! Text is synced both ways. A copied file list (Explorer's Copy on the
//! agent) is sent as its paths; the viewer puts them on its clipboard as
//! text, since the files themselves are not transferred.

use serde::{Deserialize, Serialize};

/// Largest clipboard content synced, in bytes of text. Bigger copies stay
/// local.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardData {
    Text(String),
    /// Full paths of copied files (on the machine they were copied on).
    Files(Vec<String>),
}

impl ClipboardData {
    /// Size counted against [`MAX_CLIPBOARD_BYTES`].
    pub fn size(&self) -> usize {
        match self {
            ClipboardData::Text(t) => t.len(),
            ClipboardData::Files(paths) => paths.iter().map(|p| p.len() + 1).sum(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            ClipboardData::Text(t) => t.is_empty(),
            ClipboardData::Files(paths) => paths.is_empty(),
        }
    }

    /// As plain text: file lists become one path per line.
    pub fn to_text(&self) -> String {
        match self {
            ClipboardData::Text(t) => t.clone(),
            ClipboardData::Files(paths) => paths.join("\n"),
        }
    }
}

/// Loop guard and size cap for one end of the sync.
#[derive(Debug, Default)]
pub struct ClipboardGuard {
    /// The content both sides are known to have: last sent or last applied.
    last: Option<ClipboardData>,
}

impl ClipboardGuard {
    /// Record what is on the local clipboard at start-up, without sending it:
    /// connecting must not leak whatever happened to be copied earlier.
    pub fn prime(&mut self, current: Option<ClipboardData>) {
        self.last = current;
    }

    /// The local clipboard changed to `data`. Returns what to send, or `None`
    /// for an echo of the last synced content, empty content, or content
    /// over the size cap.
    pub fn local_changed(&mut self, data: ClipboardData) -> Option<ClipboardData> {
        if data.is_empty() || data.size() > MAX_CLIPBOARD_BYTES {
            return None;
        }
        if self.last.as_ref() == Some(&data) {
            return None;
        }
        self.last = Some(data.clone());
        Some(data)
    }

    /// `data` arrived from the other end. Returns whether to write it to the
    /// local clipboard. Once written, the resulting local change is an echo.
    pub fn remote(&mut self, data: &ClipboardData) -> bool {
        if data.is_empty() || data.size() > MAX_CLIPBOARD_BYTES {
            return false;
        }
        if self.last.as_ref() == Some(data) {
            return false;
        }
        self.last = Some(data.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;

    fn text(s: &str) -> ClipboardData {
        ClipboardData::Text(s.to_owned())
    }

    #[test]
    fn clipboard_messages_round_trip_through_postcard() {
        let files = ClipboardData::Files(vec![
            r"C:\Users\claude\report.docx".into(),
            r"C:\temp\ünïcode.txt".into(),
        ]);
        for data in [text("hello\r\nworld"), files] {
            for msg in [
                Message::Clipboard(data.clone()),
                Message::SessionClipboard {
                    session_id: 3,
                    data: data.clone(),
                },
            ] {
                let bytes = postcard::to_stdvec(&msg).unwrap();
                assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), msg);
            }
        }
    }

    #[test]
    fn a_full_size_clipboard_fits_in_one_frame() {
        let msg = Message::Clipboard(text(&"x".repeat(MAX_CLIPBOARD_BYTES)));
        assert!(postcard::to_stdvec(&msg).unwrap().len() < crate::MAX_FRAME_LEN as usize);
    }

    #[test]
    fn two_ends_do_not_bounce_a_copy_back_and_forth() {
        let mut viewer = ClipboardGuard::default();
        let mut agent = ClipboardGuard::default();

        // Technician copies on the viewer.
        let sent = viewer.local_changed(text("from viewer")).unwrap();
        assert!(agent.remote(&sent), "agent applies it");
        // Applying it fires the agent's clipboard listener: not sent back.
        assert_eq!(agent.local_changed(sent.clone()), None);

        // User copies on the agent.
        let back = agent.local_changed(text("from agent")).unwrap();
        assert!(viewer.remote(&back));
        assert_eq!(
            viewer.local_changed(back),
            None,
            "viewer's poll sees an echo"
        );

        // Copying the earlier text again is a real change.
        assert_eq!(
            viewer.local_changed(text("from viewer")),
            Some(text("from viewer"))
        );
    }

    #[test]
    fn repeated_local_reports_are_sent_once() {
        let mut g = ClipboardGuard::default();
        assert!(g.local_changed(text("a")).is_some());
        assert!(g.local_changed(text("a")).is_none());
        assert!(!g.remote(&text("a")), "peer already has it");
    }

    #[test]
    fn primed_content_is_not_sent_on_connect() {
        let mut g = ClipboardGuard::default();
        g.prime(Some(text("password123 copied yesterday")));
        assert_eq!(g.local_changed(text("password123 copied yesterday")), None);
        assert!(g.local_changed(text("new")).is_some());
    }

    #[test]
    fn oversized_and_empty_content_is_not_synced() {
        let mut g = ClipboardGuard::default();
        let big = text(&"x".repeat(MAX_CLIPBOARD_BYTES + 1));
        assert_eq!(g.local_changed(big.clone()), None);
        assert!(!g.remote(&big));
        assert_eq!(g.local_changed(text("")), None);
        assert_eq!(g.local_changed(ClipboardData::Files(vec![])), None);
        let exactly = text(&"x".repeat(MAX_CLIPBOARD_BYTES));
        assert!(g.local_changed(exactly).is_some());
    }

    #[test]
    fn file_lists_become_one_path_per_line() {
        let files = ClipboardData::Files(vec![r"C:\a.txt".into(), r"C:\b.txt".into()]);
        assert_eq!(files.to_text(), "C:\\a.txt\nC:\\b.txt");
        assert_eq!(files.size(), 18);
    }
}
