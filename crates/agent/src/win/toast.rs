//! Windows toast notifications from the helper ("Jane Doe connected to this
//! computer").
//!
//! An unpackaged program needs an AppUserModelID that Windows knows before
//! it may show toasts. Registering one under
//! `HKCU\Software\Classes\AppUserModelId\<id>` with a display name is
//! enough (the same thing the Windows App SDK does for unpackaged apps).
//! The helper runs as the user, so it can write that key itself.

use tracing::{info, warn};
use windows::core::{Result, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

/// Our AppUserModelID; the toast's source line shows [`DISPLAY_NAME`].
const APP_ID: &str = "RMM.Agent";
const DISPLAY_NAME: &str = "RMM Agent";

/// Register the AppUserModelID (idempotent). Call once at helper start.
pub fn register() {
    if let Err(e) = register_app_id() {
        warn!("registering toast app id: {e}");
    }
}

fn register_app_id() -> Result<()> {
    let path = HSTRING::from(format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    let mut key = HKEY::default();
    // SAFETY: creating/opening a key under HKCU with an owned path; the
    // value is a NUL-terminated UTF-16 string of the stated length.
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &path,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let name: Vec<u16> = DISPLAY_NAME.encode_utf16().chain([0]).collect();
        let bytes = std::slice::from_raw_parts(name.as_ptr().cast::<u8>(), name.len() * 2);
        let set = RegSetValueExW(
            key,
            &HSTRING::from("DisplayName"),
            None,
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(key);
        set.ok()
    }
}

/// Show "`technician` connected to this computer". Runs on a short-lived
/// thread of its own (WinRT needs an initialised apartment).
pub fn technician_connected(technician: &str) {
    let technician = technician.to_owned();
    std::thread::spawn(move || match show(&technician) {
        Ok(()) => info!(%technician, "toast shown"),
        Err(e) => warn!(%technician, "showing toast: {e}"),
    });
}

fn show(technician: &str) -> Result<()> {
    // SAFETY: initialises WinRT for this new thread.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\">\
         <text>Remote support session started</text>\
         <text>{} connected to this computer.</text>\
         <text>Press Ctrl+F12 to end all remote sessions.</text>\
         </binding></visual></toast>",
        escape(technician)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)
}

/// Minimal XML text escaping for a user name.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_escaped() {
        assert_eq!(super::escape("A&B <x>"), "A&amp;B &lt;x&gt;");
    }
}
