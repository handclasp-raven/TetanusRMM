//! Which desktop is on screen, and following it.
//!
//! A session has several desktops, and one of them at a time receives
//! input and is shown: `Default`, where the user's programs run, or
//! `Winlogon`, the secure desktop, which holds the logon screen, the lock
//! screen and UAC prompts. A thread captures and injects input on the
//! desktop it is attached to, and only SYSTEM may attach to `Winlogon`.
//! So the system helper's threads re-attach whenever the input desktop
//! changes ([`Follower`]); the session helper stays on `Default`.

use windows::Win32::Foundation::{GENERIC_ALL, HANDLE};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW, WTS_SESSIONSTATE_LOCK,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

/// The desktop the user's programs run on.
const USER_DESKTOP: &str = "Default";

/// Whether the desktop called `name` is one only SYSTEM can reach (the
/// logon screen, the lock screen, a UAC prompt, the screen saver).
pub fn is_secure(name: &str) -> bool {
    !name.eq_ignore_ascii_case(USER_DESKTOP)
}

struct Desktop(HDESK);

impl Desktop {
    /// The desktop receiving input now, in this process's session.
    fn input() -> windows::core::Result<Self> {
        // SAFETY: no pointers; the handle is owned by the returned guard.
        unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )
        }
        .map(Self)
    }

    fn name(&self) -> windows::core::Result<String> {
        let mut name = [0u16; 256];
        let mut needed = 0;
        // SAFETY: the buffer is as long as the size passed.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(self.0 .0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                std::mem::size_of_val(&name) as u32,
                Some(&mut needed),
            )?
        };
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Ok(String::from_utf16_lossy(&name[..len]))
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // SAFETY: we own the handle. Fails (harmlessly) while a thread is
        // still attached to it.
        unsafe {
            let _ = CloseDesktop(self.0);
        }
    }
}

/// Keeps the thread that owns it on the input desktop. Create and use it
/// on one thread, which must have no windows or hooks (Windows refuses to
/// move a thread that has).
#[derive(Default)]
pub struct Follower {
    attached: Option<(Desktop, String)>,
}

impl Follower {
    /// Attach this thread to the desktop receiving input now, if it is
    /// not already there. Returns that desktop's name.
    ///
    /// Fails for a moment while Windows is switching desktops; the thread
    /// then stays where it was, and the caller tries again.
    pub fn follow(&mut self) -> windows::core::Result<&str> {
        let desktop = Desktop::input()?;
        let name = desktop.name()?;
        if !self.attached.as_ref().is_some_and(|(_, n)| *n == name) {
            // SAFETY: valid desktop handle, kept open while attached.
            unsafe { SetThreadDesktop(desktop.0)? };
            // The old handle is closed now that the thread has left it.
            self.attached = Some((desktop, name));
        }
        Ok(self.attached.as_ref().map_or("", |(_, n)| n))
    }
}

/// Whether `session_id`'s screen is locked (a user is logged on, but the
/// lock screen is up).
pub fn session_locked(session_id: u32) -> bool {
    let mut buffer = windows::core::PWSTR::null();
    let mut bytes = 0;
    // SAFETY: out-parameters; the buffer is freed below.
    let queried = unsafe {
        WTSQuerySessionInformationW(None, session_id, WTSSessionInfoEx, &mut buffer, &mut bytes)
    };
    if queried.is_err() || buffer.is_null() {
        return false;
    }
    let locked = if bytes as usize >= std::mem::size_of::<WTSINFOEXW>() {
        // SAFETY: Windows returned at least a WTSINFOEXW; level 1 is the
        // only level there is, and is checked before the union is read.
        unsafe {
            let info = buffer.0.cast::<WTSINFOEXW>().read_unaligned();
            info.Level == 1
                && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32
        }
    } else {
        false
    };
    // SAFETY: allocated by WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    locked
}
