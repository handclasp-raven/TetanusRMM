//! What kind of machine the agent runs on, reported to the server so a new
//! device gets the right default consent mode (workstation: `notify`,
//! server: `unattended`), along with its hostname, OS and DNS servers for
//! display.

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

/// The operating system's name and version for display, e.g. `Windows 11
/// Pro (build 26100)` or `Linux (Ubuntu 24.04)`. `None` if the OS does not
/// say.
pub fn os() -> Option<String> {
    use sysinfo::System;

    let long = System::long_os_version();
    let described = if cfg!(windows) {
        // The product name has no build number; the kernel version is it.
        match (long, System::kernel_version()) {
            (Some(name), Some(build)) => Some(format!("{name} (build {build})")),
            (name, _) => name,
        }
    } else {
        long
    };
    described
        .or_else(|| match (System::name(), System::os_version()) {
            (Some(name), Some(version)) => Some(format!("{name} {version}")),
            (name, _) => name,
        })
        .filter(|os| !os.trim().is_empty())
}

/// DNS servers configured on the machine, in the order they are used and
/// without duplicates. Windows: those of every network adapter that is up.
#[cfg(windows)]
pub fn dns_servers() -> Vec<std::net::IpAddr> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME,
        GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    let flags = GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_FRIENDLY_NAME;
    let mut size = 16 * 1024u32;
    // u64s: the adapter list needs 8-byte alignment.
    let mut buffer: Vec<u64>;
    let mut attempts = 0;
    loop {
        buffer = vec![0; (size as usize).div_ceil(8)];
        // SAFETY: `buffer` holds `size` bytes; the call fills it with a
        // linked list of adapters pointing into itself.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC.0),
                flags,
                None,
                Some(buffer.as_mut_ptr().cast()),
                &mut size,
            )
        };
        attempts += 1;
        if result == ERROR_BUFFER_OVERFLOW.0 && attempts < 4 {
            continue; // `size` now says how much is needed
        }
        if result != NO_ERROR.0 {
            return Vec::new();
        }
        break;
    }
    let mut servers = Vec::new();
    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !adapter.is_null() {
        // SAFETY: a node of the list GetAdaptersAddresses built in `buffer`.
        let a = unsafe { &*adapter };
        if a.OperStatus == IfOperStatusUp {
            let mut dns = a.FirstDnsServerAddress;
            while !dns.is_null() {
                // SAFETY: as above; the socket address is the family it says.
                let d = unsafe { &*dns };
                let address = d.Address.lpSockaddr;
                let ip = if address.is_null() {
                    None
                } else {
                    match unsafe { (*address).sa_family } {
                        AF_INET => {
                            let v4 = unsafe { &*address.cast::<SOCKADDR_IN>() };
                            // Stored in network byte order.
                            Some(std::net::IpAddr::from(
                                unsafe { v4.sin_addr.S_un.S_addr }.to_ne_bytes(),
                            ))
                        }
                        AF_INET6 => {
                            let v6 = unsafe { &*address.cast::<SOCKADDR_IN6>() };
                            Some(std::net::IpAddr::from(unsafe { v6.sin6_addr.u.Byte }))
                        }
                        _ => None,
                    }
                };
                if let Some(ip) = ip.filter(|ip| !is_placeholder_dns(ip)) {
                    if !servers.contains(&ip) {
                        servers.push(ip);
                    }
                }
                dns = d.Next;
            }
        }
        adapter = a.Next;
    }
    servers
}

/// Windows lists `fec0:0:0:ffff::1` to `::3` (deprecated site-local
/// addresses) on adapters with no IPv6 DNS configured: not real servers.
fn is_placeholder_dns(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            s[..4] == [0xfec0, 0, 0, 0xffff] && s[4..7] == [0, 0, 0] && (1..=3).contains(&s[7])
        }
        std::net::IpAddr::V4(_) => false,
    }
}

/// DNS servers configured on the machine. Elsewhere: `resolv.conf`
/// (systemd-resolved's upstream list rather than its local stub, if it
/// runs).
#[cfg(not(windows))]
pub fn dns_servers() -> Vec<std::net::IpAddr> {
    ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| parse_resolv_conf(&text))
        .unwrap_or_default()
}

/// The `nameserver` lines of a `resolv.conf`, in order, without duplicates.
#[cfg_attr(windows, allow(dead_code))]
fn parse_resolv_conf(text: &str) -> Vec<std::net::IpAddr> {
    let mut servers = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("nameserver") {
            continue;
        }
        // IPv6 link-local servers carry a zone: fe80::1%eth0.
        let Some(ip) = words
            .next()
            .and_then(|w| w.split('%').next())
            .and_then(|w| w.parse().ok())
        else {
            continue;
        };
        if !servers.contains(&ip) && !is_placeholder_dns(&ip) {
            servers.push(ip);
        }
    }
    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolv_conf_nameservers_in_order() {
        let text = "# Generated\nsearch corp.example\nnameserver 192.168.1.1\n\
                    nameserver  fe80::1%eth0\n;nameserver 9.9.9.9\nnameserver 192.168.1.1\n\
                    nameserver bogus\nnameserver 2001:db8::53\n";
        let servers: Vec<String> = parse_resolv_conf(text)
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(servers, ["192.168.1.1", "fe80::1", "2001:db8::53"]);
    }

    #[test]
    fn windows_placeholder_dns_is_not_a_server() {
        for ip in ["fec0:0:0:ffff::1", "fec0:0:0:ffff::3"] {
            assert!(is_placeholder_dns(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["fec0:0:0:ffff::4", "2001:db8::1", "10.0.0.1"] {
            assert!(!is_placeholder_dns(&ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn this_machine_lists_dns_servers() {
        // May legitimately be empty (no network); must not panic.
        println!("dns: {:?}", dns_servers());
    }

    #[test]
    fn this_machine_names_its_os() {
        let os = os().expect("an OS description");
        println!("os: {os}");
        assert!(!os.trim().is_empty());
    }

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
