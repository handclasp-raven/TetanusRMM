//! Finding the logged-on console user and starting the helper in their session.

use std::ffi::c_void;
use std::path::Path;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
};

/// No session is attached to the physical console (e.g. mid-switch).
const NO_CONSOLE_SESSION: u32 = 0xFFFF_FFFF;

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// The primary token of the user logged on to `session_id`.
///
/// Requires SE_TCB_NAME, which LocalSystem has; fails with ERROR_NO_TOKEN
/// when the session has no logged-on user (e.g. the logon screen).
fn user_token(session_id: u32) -> windows::core::Result<OwnedHandle> {
    let mut token = HANDLE::default();
    // SAFETY: out-parameter; the handle is owned by the returned guard.
    unsafe { WTSQueryUserToken(session_id, &mut token)? };
    Ok(OwnedHandle(token))
}

/// The console session, if a user is logged on to it.
///
/// A console session exists even at the logon screen, so the session id
/// alone is not enough: we also need a user token for it.
pub fn console_user_session() -> Option<u32> {
    // SAFETY: no arguments; always safe to call.
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == NO_CONSOLE_SESSION {
        return None;
    }
    user_token(session_id).ok().map(|_| session_id)
}

/// A helper process started with [`spawn_in_session`].
pub struct HelperProcess {
    process: OwnedHandle,
    thread: OwnedHandle,
    pub pid: u32,
    pub session_id: u32,
}

// SAFETY: process/thread handles may be used from any thread.
unsafe impl Send for HelperProcess {}

impl HelperProcess {
    /// Let the (suspended) process start running.
    pub fn resume(&self) {
        // SAFETY: valid thread handle from CreateProcessAsUserW.
        unsafe {
            ResumeThread(self.thread.0);
        }
    }

    pub fn is_running(&self) -> bool {
        // SAFETY: valid process handle.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }

    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0;
        // SAFETY: valid process handle; out-parameter.
        unsafe { GetExitCodeProcess(self.process.0, &mut code).ok()? };
        Some(code)
    }

    pub fn terminate(&self) {
        // SAFETY: valid process handle.
        unsafe {
            let _ = TerminateProcess(self.process.0, 0);
        }
    }
}

struct EnvironmentBlock(*mut c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        // SAFETY: allocated by CreateEnvironmentBlock.
        unsafe {
            let _ = DestroyEnvironmentBlock(self.0);
        }
    }
}

/// Start `exe args` as the user logged on to `session_id`, on their
/// interactive desktop. The process starts suspended: record its pid, then
/// call [`HelperProcess::resume`].
///
/// It runs with the *user's* token rather than SYSTEM's. That is what a
/// process on the user's desktop should have, and it keeps a compromised
/// helper from being a SYSTEM process in an unprivileged session. (Driving
/// the secure desktop for UAC and the logon screen needs a SYSTEM token in
/// the session; that is Phase 9.)
pub fn spawn_in_session(
    session_id: u32,
    exe: &Path,
    args: &str,
) -> windows::core::Result<HelperProcess> {
    let token = user_token(session_id)?;

    // The user's environment (USERPROFILE, LOCALAPPDATA, ...), not SYSTEM's.
    let mut env = std::ptr::null_mut();
    // SAFETY: out-parameter, freed by the guard.
    unsafe { CreateEnvironmentBlock(&mut env, Some(token.0), false)? };
    let env = EnvironmentBlock(env);

    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut command_line: Vec<u16> = format!("\"{}\" {args}", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut info = PROCESS_INFORMATION::default();

    // SAFETY: all pointers reference locals that outlive the call; the
    // command line buffer is mutable as CreateProcessAsUserW requires.
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            &HSTRING::from(exe.as_os_str()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            Some(env.0),
            PCWSTR::null(),
            &startup,
            &mut info,
        )?;
    }
    Ok(HelperProcess {
        process: OwnedHandle(info.hProcess),
        thread: OwnedHandle(info.hThread),
        pid: info.dwProcessId,
        session_id,
    })
}
