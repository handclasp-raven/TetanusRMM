//! What kind of machine the agent runs on, reported to the server so a new
//! device gets the right default consent mode (workstation: `notify`,
//! server: `unattended`), along with its hostname for display.

use protocol::consent::DeviceKind;

/// Windows: the OS product type (workstation vs. server or domain
/// controller).
#[cfg(windows)]
pub fn kind() -> DeviceKind {
    use windows::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOEXW};
    use windows::Win32::System::SystemServices::VER_NT_WORKSTATION;

    let mut info = OSVERSIONINFOEXW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOEXW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized OSVERSIONINFOEXW. GetVersionExW
    // may report an older version number to unmanifested processes, but the
    // product type is always accurate.
    let ok = unsafe { GetVersionExW((&mut info as *mut OSVERSIONINFOEXW).cast()) }.is_ok();
    kind_from_product_type(
        ok.then_some(u32::from(info.wProductType)),
        VER_NT_WORKSTATION,
    )
}

#[cfg(windows)]
fn kind_from_product_type(product_type: Option<u32>, workstation: u32) -> DeviceKind {
    match product_type {
        Some(t) if t != workstation => DeviceKind::Server,
        _ => DeviceKind::Workstation,
    }
}

/// Elsewhere (development builds): a machine with a graphical session is a
/// workstation, a headless one a server.
#[cfg(not(windows))]
pub fn kind() -> DeviceKind {
    kind_from_env(|name| std::env::var_os(name).is_some())
}

#[cfg(not(windows))]
fn kind_from_env(has: impl Fn(&str) -> bool) -> DeviceKind {
    if has("DISPLAY") || has("WAYLAND_DISPLAY") {
        DeviceKind::Workstation
    } else {
        DeviceKind::Server
    }
}

/// The machine's hostname (Windows: the NetBIOS computer name), cleaned for
/// display. `None` if the OS does not report one.
pub fn hostname() -> Option<String> {
    sysinfo::System::host_name().and_then(|name| protocol::sanitize_hostname(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_has_a_displayable_hostname() {
        let name = hostname().expect("a hostname");
        assert_eq!(protocol::sanitize_hostname(&name).as_deref(), Some(&*name));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_graphical_session_means_workstation() {
        assert_eq!(kind_from_env(|v| v == "DISPLAY"), DeviceKind::Workstation);
        assert_eq!(
            kind_from_env(|v| v == "WAYLAND_DISPLAY"),
            DeviceKind::Workstation
        );
        assert_eq!(kind_from_env(|_| false), DeviceKind::Server);
    }

    #[cfg(windows)]
    #[test]
    fn product_type_maps_to_kind() {
        // VER_NT_WORKSTATION = 1, VER_NT_DOMAIN_CONTROLLER = 2, VER_NT_SERVER = 3.
        assert_eq!(kind_from_product_type(Some(1), 1), DeviceKind::Workstation);
        assert_eq!(kind_from_product_type(Some(2), 1), DeviceKind::Server);
        assert_eq!(kind_from_product_type(Some(3), 1), DeviceKind::Server);
        assert_eq!(kind_from_product_type(None, 1), DeviceKind::Workstation);
        // This machine answers something.
        let _ = kind();
    }
}
