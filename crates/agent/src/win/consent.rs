//! The blocking consent prompt (`require` mode): a topmost Yes/No message
//! box on the user's desktop. It runs on its own thread so the tray, the
//! Ctrl+F12 hotkey and other prompts keep working. A timer thread closes it
//! as "no" when the timeout passes (answer: timed out) or when the server
//! withdraws the request (no answer is sent).

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use protocol::consent::PromptAnswer;
use tracing::{info, warn};
use windows::core::{BOOL, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, MessageBoxW, PostMessageW, IDNO, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION,
    MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, WM_COMMAND,
};

const OPEN: u8 = 0;
const TIMED_OUT: u8 = 1;
const CANCELLED: u8 = 2;

enum Control {
    Answered,
    Cancel,
}

/// A prompt on screen. Dropping it does not close it; [`Prompt::cancel`] does.
pub struct Prompt {
    control: mpsc::Sender<Control>,
}

impl Prompt {
    /// Withdraw the prompt (the technician left, or the user pressed Ctrl+F12).
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }
}

/// Show the prompt for `technician`. `answer` is called once with the
/// user's answer, unless the prompt is cancelled.
pub fn show(
    technician: &str,
    timeout: Duration,
    answer: impl FnOnce(PromptAnswer) + Send + 'static,
) -> Prompt {
    let (control, control_rx) = mpsc::channel();
    let state = Arc::new(AtomicU8::new(OPEN));
    let thread_id = Arc::new(AtomicU32::new(0));
    let text = format!(
        "{technician} from your IT support team wants to connect to this computer \
         and control it remotely.\n\n\
         Allow the remote session?\n\n\
         If you do not answer within {} seconds, the request is declined.\n\
         You can end remote sessions at any time with Ctrl+F12.",
        timeout.as_secs()
    );

    std::thread::Builder::new()
        .name("consent-prompt".into())
        .spawn({
            let state = state.clone();
            let thread_id = thread_id.clone();
            let control = control.clone();
            move || {
                // SAFETY: plain thread id query.
                thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
                // SAFETY: modal message box with owned strings; blocks this
                // thread only.
                let result = unsafe {
                    MessageBoxW(
                        None,
                        &HSTRING::from(text),
                        &HSTRING::from("Remote support request"),
                        MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
                    )
                };
                let _ = control.send(Control::Answered);
                let reply = match (result, state.load(Ordering::SeqCst)) {
                    (_, CANCELLED) => None,
                    (IDYES, _) => Some(PromptAnswer::Accepted),
                    (_, TIMED_OUT) => Some(PromptAnswer::TimedOut),
                    _ => Some(PromptAnswer::Declined),
                };
                info!(?result, ?reply, "consent prompt closed");
                if let Some(reply) = reply {
                    answer(reply);
                }
            }
        })
        .expect("spawn consent prompt thread");

    std::thread::Builder::new()
        .name("consent-timer".into())
        .spawn(move || {
            let close_as = match control_rx.recv_timeout(timeout) {
                Ok(Control::Answered) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Control::Cancel) => CANCELLED,
                Err(RecvTimeoutError::Timeout) => TIMED_OUT,
            };
            state.store(close_as, Ordering::SeqCst);
            close_windows_of(thread_id.load(Ordering::SeqCst));
        })
        .expect("spawn consent timer thread");

    Prompt { control }
}

/// Press "No" on the message box owned by `thread_id`.
fn close_windows_of(thread_id: u32) {
    extern "system" fn press_no(hwnd: HWND, _: LPARAM) -> BOOL {
        // SAFETY: posting a command to a window we enumerated.
        if let Err(e) =
            unsafe { PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(IDNO.0 as usize), LPARAM(0)) }
        {
            warn!("closing consent prompt: {e}");
        }
        true.into()
    }
    if thread_id != 0 {
        // SAFETY: enumerates that thread's windows with a valid callback.
        let _ = unsafe { EnumThreadWindows(thread_id, Some(press_no), LPARAM(0)) };
    }
}
