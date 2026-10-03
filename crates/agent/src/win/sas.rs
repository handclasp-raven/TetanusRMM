//! Ctrl+Alt+Del for the technician.
//!
//! The secure attention sequence cannot be injected as key events:
//! Windows only takes it from the keyboard driver, or from `SendSAS`
//! called by a service running as SYSTEM. And `SendSAS` only works where
//! the "Disable or enable software Secure Attention Sequence" policy
//! lets services use it, which it does not by default. So if the policy
//! does not, it is switched on for the length of the call and then put
//! back as it was.

use tracing::{info, warn};
use windows::core::{s, w};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RRF_RT_REG_DWORD,
};

/// Bit of `SoftwareSASGeneration` that lets services send the sequence
/// (the other bit is for ease of access applications).
const SERVICES: u32 = 1;

/// Press Ctrl+Alt+Del on the console. Only works from the service.
pub fn send() {
    let policy = match Policy::open() {
        Ok(policy) => Some(policy),
        Err(e) => {
            warn!("opening the Ctrl+Alt+Del policy: {e}");
            None
        }
    };
    let previous = policy.as_ref().map(Policy::get);
    let allowed = previous.is_some_and(|v| v.unwrap_or(0) & SERVICES != 0);
    let mut changed = false;
    if let (Some(policy), false) = (&policy, allowed) {
        let value = previous.flatten().unwrap_or(0) | SERVICES;
        match policy.set(Some(value)) {
            Ok(()) => changed = true,
            Err(e) => warn!("allowing a software Ctrl+Alt+Del: {e}"),
        }
    }
    match send_sas() {
        Ok(()) => info!(allowed, "Ctrl+Alt+Del sent"),
        Err(e) => warn!("sending Ctrl+Alt+Del: {e}"),
    }
    if let (Some(policy), true) = (&policy, changed) {
        if let Err(e) = policy.set(previous.flatten()) {
            warn!("restoring the Ctrl+Alt+Del policy: {e}");
        }
    }
}

fn send_sas() -> windows::core::Result<()> {
    // SAFETY: sas.dll is loaded from System32 only (and left loaded), and
    // `SendSAS` is `void SendSAS(BOOL AsUser)`.
    unsafe {
        let library = LoadLibraryExW(w!("sas.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)?;
        let send =
            GetProcAddress(library, s!("SendSAS")).ok_or_else(windows::core::Error::from_thread)?;
        let send: unsafe extern "system" fn(i32) = std::mem::transmute(send);
        // FALSE: the caller is a service, not the user's own program.
        send(0);
    }
    Ok(())
}

fn check(error: WIN32_ERROR) -> windows::core::Result<()> {
    if error == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(error.into())
    }
}

/// The `SoftwareSASGeneration` policy value.
struct Policy(HKEY);

impl Policy {
    const VALUE: windows::core::PCWSTR = w!("SoftwareSASGeneration");

    fn open() -> windows::core::Result<Self> {
        let mut key = HKEY::default();
        // SAFETY: constant strings; out-parameter owned by the guard.
        check(unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System"),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        })?;
        Ok(Self(key))
    }

    /// The value, or `None` if it is not set.
    fn get(&self) -> Option<u32> {
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: the buffer is a u32 and `size` says so.
        let error = unsafe {
            RegGetValueW(
                self.0,
                None,
                Self::VALUE,
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::from_mut(&mut value).cast()),
                Some(&mut size),
            )
        };
        (error == ERROR_SUCCESS).then_some(value)
    }

    /// Set the value, or remove it.
    fn set(&self, value: Option<u32>) -> windows::core::Result<()> {
        // SAFETY: valid key; the data is a u32's bytes.
        unsafe {
            match value {
                Some(value) => check(RegSetValueExW(
                    self.0,
                    Self::VALUE,
                    None,
                    REG_DWORD,
                    Some(&value.to_le_bytes()),
                )),
                None => match RegDeleteValueW(self.0, Self::VALUE) {
                    ERROR_FILE_NOT_FOUND => Ok(()),
                    error => check(error),
                },
            }
        }
    }
}

impl Drop for Policy {
    fn drop(&mut self) {
        // SAFETY: we own the key.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
