//! The prompt for a password the user lends the technicians (see
//! `protocol::credential`): a small topmost dialog on the user's desktop
//! naming the technician, with one masked field.
//!
//! Like the consent prompt (see `super::consent`) it runs on its own
//! thread, and a timer thread closes it as "no" when nobody answers or the
//! request is withdrawn.
//!
//! What is typed leaves this process only as a [`Secret`] for the service,
//! which keeps it (see `super::secret`). The copies made here are wiped;
//! the edit control's own buffer is emptied before the dialog closes.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use protocol::credential::MAX_PASSWORD_UNITS;
use protocol::ipc::Secret;
use tracing::{info, warn};
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DialogBoxIndirectParamW, EndDialog, EnumThreadWindows, GetDlgItem,
    GetDlgItemTextW, GetForegroundWindow, GetWindowThreadProcessId, KillTimer, PostMessageW,
    SendDlgItemMessageW, SetDlgItemTextW, SetForegroundWindow, SetTimer, BS_DEFPUSHBUTTON,
    DLGTEMPLATE, DS_CENTER, DS_MODALFRAME, DS_SETFONT, DS_SETFOREGROUND, ES_AUTOHSCROLL,
    ES_PASSWORD, IDCANCEL, IDOK, WM_COMMAND, WM_INITDIALOG, WM_TIMER, WS_BORDER, WS_CAPTION,
    WS_CHILD, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use zeroize::Zeroize;

/// How long the user has to answer.
const TIMEOUT: Duration = Duration::from_secs(120);

/// Longest technician name shown.
const NAME_CHARS: usize = 64;

/// Timer that takes the keyboard once the dialog is on screen.
const FOCUS_TIMER: usize = 1;

/// The password field's control id.
const ID_PASSWORD: i32 = 100;

/// `EM_LIMITTEXT`, `SS_NOPREFIX` and the predefined control classes of a
/// dialog template (from winuser.h).
const EM_LIMITTEXT: u32 = 0x00C5;
const SS_NOPREFIX: u32 = 0x0080;
const CLASS_BUTTON: u16 = 0x0080;
const CLASS_EDIT: u16 = 0x0081;
const CLASS_STATIC: u16 = 0x0082;

thread_local! {
    /// What the user typed, from the dialog procedure to [`show`]'s thread
    /// (the dialog runs on it).
    static TYPED: RefCell<Option<Secret>> = const { RefCell::new(None) };
    /// Set when this thread's prompt is withdrawn or timed out.
    static CLOSED: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

enum Control {
    Answered,
    Cancel,
}

/// A prompt on screen. Dropping it does not close it; [`Prompt::cancel`] does.
pub struct Prompt {
    control: mpsc::Sender<Control>,
}

impl Prompt {
    /// Withdraw the prompt (the technicians left, or the user pressed
    /// Ctrl+F12).
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }
}

/// Ask for a password on behalf of `technician`. `answer` is called once:
/// with what the user typed, or `None` if they declined, did not answer in
/// time, or the prompt was withdrawn.
pub fn show(technician: &str, answer: impl FnOnce(Option<Secret>) + Send + 'static) -> Prompt {
    let (control, control_rx) = mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    let thread_id = Arc::new(AtomicU32::new(0));
    let name: String = technician.chars().take(NAME_CHARS).collect();
    let text = format!(
        "{name} from your IT support team asks you to type a password they can use on \
         this computer while you are away, for example to unlock it.\n\n\
         It stays on this computer, is never shown to them, and is forgotten when \
         remote support ends or you press Ctrl+F12."
    );

    std::thread::Builder::new()
        .name("credential-prompt".into())
        .spawn({
            let closed = closed.clone();
            let thread_id = thread_id.clone();
            let control = control.clone();
            move || {
                // SAFETY: plain thread id query.
                thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
                CLOSED.set(Some(closed.clone()));
                let template = template(&text);
                // SAFETY: the template is a well-formed, DWORD-aligned
                // DLGTEMPLATE that outlives the call; modal, so it blocks
                // this thread only.
                let result = unsafe {
                    let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
                    DialogBoxIndirectParamW(
                        module,
                        template.as_ptr().cast::<DLGTEMPLATE>(),
                        None,
                        Some(dialog_proc),
                        LPARAM(0),
                    )
                };
                let _ = control.send(Control::Answered);
                let typed = TYPED.take().filter(|_| !closed.load(Ordering::SeqCst));
                info!(result, typed = typed.is_some(), "password prompt closed");
                answer(typed);
            }
        })
        .expect("spawn credential prompt thread");

    std::thread::Builder::new()
        .name("credential-timer".into())
        .spawn(move || {
            match control_rx.recv_timeout(TIMEOUT) {
                Ok(Control::Answered) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Control::Cancel) | Err(RecvTimeoutError::Timeout) => {}
            }
            closed.store(true, Ordering::SeqCst);
            close_windows_of(thread_id.load(Ordering::SeqCst));
        })
        .expect("spawn credential timer thread");

    Prompt { control }
}

extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wparam: WPARAM, _: LPARAM) -> isize {
    match msg {
        WM_INITDIALOG => {
            // SAFETY: messages to our own dialog and its controls.
            unsafe {
                SendDlgItemMessageW(
                    hwnd,
                    ID_PASSWORD,
                    EM_LIMITTEXT,
                    WPARAM(MAX_PASSWORD_UNITS),
                    LPARAM(0),
                );
                // Withdrawn before it was on screen.
                if CLOSED.with_borrow(|c| c.as_ref().is_some_and(|c| c.load(Ordering::SeqCst))) {
                    let _ = EndDialog(hwnd, 0);
                }
                SetTimer(Some(hwnd), FOCUS_TIMER, 50, None);
            }
            // Let the dialog manager focus the first control, the field.
            1
        }
        WM_TIMER if wparam.0 == FOCUS_TIMER => {
            // SAFETY: our own dialog and timer.
            unsafe {
                let _ = KillTimer(Some(hwnd), FOCUS_TIMER);
            }
            take_keyboard(hwnd);
            1
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            if id == IDOK.0 {
                let mut buf = [0u16; MAX_PASSWORD_UNITS + 1];
                // SAFETY: reads our own control's text into `buf`.
                let len = unsafe { GetDlgItemTextW(hwnd, ID_PASSWORD, &mut buf) } as usize;
                if len == 0 {
                    // Nothing typed: stay open.
                    return 1;
                }
                TYPED.set(Some(Secret::new(
                    buf[..len.min(MAX_PASSWORD_UNITS)].to_vec(),
                )));
                buf.zeroize();
                // SAFETY: our own dialog.
                unsafe {
                    let _ = SetDlgItemTextW(hwnd, ID_PASSWORD, w!(""));
                    let _ = EndDialog(hwnd, 1);
                }
                1
            } else if id == IDCANCEL.0 {
                // SAFETY: our own dialog.
                unsafe {
                    let _ = SetDlgItemTextW(hwnd, ID_PASSWORD, w!(""));
                    let _ = EndDialog(hwnd, 0);
                }
                1
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// Make the dialog the foreground window with the caret in the field, so
/// that what the user types next goes into it. Windows only lets the
/// foreground application's threads do that, and this helper is a
/// background process: so for the moment of the call, this thread shares
/// the input state of the thread that owns the foreground window.
fn take_keyboard(hwnd: HWND) {
    // SAFETY: plain window and thread queries; the attachment is undone
    // before returning.
    unsafe {
        let me = GetCurrentThreadId();
        let foreground = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = foreground != 0
            && foreground != me
            && AttachThreadInput(me, foreground, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let taken = SetForegroundWindow(hwnd).as_bool();
        if let Ok(field) = GetDlgItem(Some(hwnd), ID_PASSWORD) {
            let _ = SetFocus(Some(field));
        }
        if attached {
            let _ = AttachThreadInput(me, foreground, false);
        }
        info!(taken, attached, "password prompt on screen");
    }
}

/// Press "Cancel" on the dialog owned by `thread_id`.
fn close_windows_of(thread_id: u32) {
    extern "system" fn press_cancel(hwnd: HWND, _: LPARAM) -> BOOL {
        // SAFETY: posting a command to a window we enumerated.
        if let Err(e) = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_COMMAND,
                WPARAM(IDCANCEL.0 as usize),
                LPARAM(0),
            )
        } {
            warn!("closing password prompt: {e}");
        }
        true.into()
    }
    if thread_id != 0 {
        // SAFETY: enumerates that thread's windows with a valid callback.
        let _ = unsafe { EnumThreadWindows(thread_id, Some(press_cancel), LPARAM(0)) };
    }
}

/// The dialog, as the in-memory template `DialogBoxIndirectParamW` takes:
/// a `DLGTEMPLATE`, then each control's `DLGITEMTEMPLATE`, all in 16-bit
/// words with every control starting on a 32-bit boundary. Returned as
/// `u32`s so the whole is aligned as the API requires. Sizes are in dialog
/// units.
fn template(text: &str) -> Vec<u32> {
    fn dword(t: &mut Vec<u16>, v: u32) {
        t.push(v as u16);
        t.push((v >> 16) as u16);
    }
    fn string(t: &mut Vec<u16>, s: &str) {
        t.extend(s.encode_utf16().filter(|&u| u != 0));
        t.push(0);
    }
    fn item(t: &mut Vec<u16>, style: u32, rect: [u16; 4], id: u16, class: u16, title: &str) {
        if t.len() % 2 != 0 {
            t.push(0);
        }
        dword(t, style | WS_CHILD.0 | WS_VISIBLE.0);
        dword(t, 0);
        t.extend(rect);
        t.push(id);
        t.extend([0xFFFF, class]);
        string(t, title);
        // No creation data.
        t.push(0);
    }

    let mut t = Vec::new();
    dword(
        &mut t,
        WS_POPUP.0
            | WS_CAPTION.0
            | WS_SYSMENU.0
            | (DS_MODALFRAME | DS_SETFONT | DS_CENTER | DS_SETFOREGROUND) as u32,
    );
    dword(&mut t, WS_EX_TOPMOST.0);
    // Four controls; position (centred, so unused) and size.
    t.extend([4, 0, 0, 260, 116]);
    // No menu, the default dialog class.
    t.extend([0, 0]);
    string(&mut t, "Remote support: lend a password");
    t.push(9);
    string(&mut t, "Segoe UI");

    // The field comes first so that it has the focus.
    item(
        &mut t,
        WS_BORDER.0 | WS_TABSTOP.0 | (ES_PASSWORD | ES_AUTOHSCROLL) as u32,
        [10, 72, 240, 14],
        ID_PASSWORD as u16,
        CLASS_EDIT,
        "",
    );
    item(
        &mut t,
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        [146, 94, 50, 14],
        IDOK.0 as u16,
        CLASS_BUTTON,
        "OK",
    );
    item(
        &mut t,
        WS_TABSTOP.0,
        [200, 94, 50, 14],
        IDCANCEL.0 as u16,
        CLASS_BUTTON,
        "Cancel",
    );
    item(
        &mut t,
        SS_NOPREFIX,
        [10, 8, 240, 60],
        0xFFFF,
        CLASS_STATIC,
        text,
    );

    if t.len() % 2 != 0 {
        t.push(0);
    }
    t.chunks_exact(2)
        .map(|w| u32::from(w[0]) | u32::from(w[1]) << 16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_in_the_template_starts_on_a_dword() {
        let words = template("Jane asks.");
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        // Four controls, and the field's class and id where expected: the
        // first item follows the header, title and font, DWORD-aligned.
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 4);
        let edit: Vec<u8> = [0xFFFF, CLASS_EDIT]
            .iter()
            .flat_map(|w: &u16| w.to_le_bytes())
            .collect();
        let at = bytes.windows(4).position(|w| w == edit).unwrap();
        // style + exstyle + rect + id = 18 bytes before the class.
        assert_eq!((at - 18) % 4, 0);
        assert_eq!(
            u16::from_le_bytes([bytes[at - 2], bytes[at - 1]]),
            ID_PASSWORD as u16
        );
    }
}
