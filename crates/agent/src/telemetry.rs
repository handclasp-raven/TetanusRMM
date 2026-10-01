//! Health telemetry (CPU, memory, system disk, uptime) sent on each
//! heartbeat, and who is signed in and every fixed disk (for the server's
//! agent status).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::Telemetry;
use sysinfo::{Disks, System};

/// Something that can produce a telemetry sample. Sampling may block briefly
/// (it reads OS counters), so callers run it on the blocking pool.
pub trait TelemetrySource: Send + Sync {
    fn sample(&self) -> Telemetry;

    /// Users signed in to the machine's interactive sessions. Default: none
    /// known.
    fn signed_in_users(&self) -> Vec<String> {
        Vec::new()
    }

    /// Fixed disks and how full they are. Default: none known.
    fn disks(&self) -> Vec<protocol::DiskUsage> {
        Vec::new()
    }
}

/// Telemetry from the live system via `sysinfo`.
pub struct SystemTelemetry {
    state: Mutex<(System, Disks)>,
}

impl SystemTelemetry {
    pub fn new() -> Self {
        let mut system = System::new();
        // CPU usage is measured between two refreshes; prime the first one so
        // the first real sample is meaningful.
        system.refresh_cpu_usage();
        Self {
            state: Mutex::new((system, Disks::new_with_refreshed_list())),
        }
    }
}

impl Default for SystemTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySource for SystemTelemetry {
    fn sample(&self) -> Telemetry {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (system, disks) = &mut *guard;
        system.refresh_cpu_usage();
        system.refresh_memory();
        disks.refresh(true);
        let disk_list = disk_spaces(disks);
        let (disk_used_bytes, disk_total_bytes) =
            system_disk_usage(&disk_list, &system_root()).unwrap_or((0, 0));
        Telemetry {
            cpu_percent: system.global_cpu_usage(),
            mem_used_bytes: system.used_memory(),
            mem_total_bytes: system.total_memory(),
            disk_used_bytes,
            disk_total_bytes,
            uptime_secs: System::uptime(),
        }
    }

    fn disks(&self) -> Vec<protocol::DiskUsage> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (_, disks) = &mut *guard;
        // Drives come and go (USB disks, mounts): list them afresh.
        disks.refresh(true);
        fixed_disks(&disk_spaces(disks))
    }

    fn signed_in_users(&self) -> Vec<String> {
        let mut users = Vec::new();
        for user in signed_in_users() {
            if !users.contains(&user) {
                users.push(user);
            }
        }
        users
    }
}

/// `DOMAIN\user` for every Windows session with a user signed in, whether
/// at the console, over remote desktop, or disconnected but still signed in.
#[cfg(windows)]
fn signed_in_users() -> Vec<String> {
    use windows::Win32::System::RemoteDesktop::{
        WTSActive, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory,
        WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
    };

    let mut sessions: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-parameters; the array is freed below.
    if unsafe {
        WTSEnumerateSessionsW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            0,
            1,
            &mut sessions,
            &mut count,
        )
    }
    .is_err()
    {
        return Vec::new();
    }
    // SAFETY: WTSEnumerateSessionsW returned `count` entries at `sessions`.
    let list = unsafe { std::slice::from_raw_parts(sessions, count as usize) };
    let users = list
        .iter()
        .filter(|s| s.State == WTSActive || s.State == WTSDisconnected)
        .filter_map(|s| session_user(s.SessionId))
        .collect();
    // SAFETY: allocated by WTSEnumerateSessionsW.
    unsafe { WTSFreeMemory(sessions.cast()) };
    users
}

/// `DOMAIN\user` signed in to Windows session `session_id`, if anyone.
#[cfg(windows)]
pub(crate) fn session_user(session_id: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSDomainName, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
        WTS_CURRENT_SERVER_HANDLE, WTS_INFO_CLASS,
    };

    fn query(session_id: u32, class: WTS_INFO_CLASS) -> Option<String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY: out-parameters; the buffer is freed below.
        unsafe {
            WTSQuerySessionInformationW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                session_id,
                class,
                &mut buffer,
                &mut bytes,
            )
            .ok()?;
            let value = buffer.to_string().ok();
            WTSFreeMemory(buffer.0.cast());
            value.filter(|v| !v.is_empty())
        }
    }

    let user = query(session_id, WTSUserName)?;
    Some(match query(session_id, WTSDomainName) {
        Some(domain) => format!("{domain}\\{user}"),
        None => user,
    })
}

/// Login names from utmpx (terminal, SSH and graphical logins).
#[cfg(unix)]
fn signed_in_users() -> Vec<String> {
    let mut users = Vec::new();
    // SAFETY: the utmpx functions are not thread-safe, but only this
    // sampler (behind its mutex) uses them; each entry is copied out
    // before the next call.
    unsafe {
        libc::setutxent();
        loop {
            let entry = libc::getutxent();
            if entry.is_null() {
                break;
            }
            let entry = &*entry;
            if entry.ut_type != libc::USER_PROCESS {
                continue;
            }
            let name: Vec<u8> = entry
                .ut_user
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            if !name.is_empty() {
                users.push(String::from_utf8_lossy(&name).into_owned());
            }
        }
        libc::endutxent();
    }
    users
}

#[cfg(not(any(windows, unix)))]
fn signed_in_users() -> Vec<String> {
    Vec::new()
}

/// One mounted disk, as far as telemetry cares.
#[derive(Debug, Clone)]
pub struct DiskSpace {
    pub mount_point: PathBuf,
    pub total: u64,
    pub available: u64,
    /// The device (`/dev/nvme0n1p2`; the volume label on Windows).
    pub device: String,
    pub file_system: String,
    pub removable: bool,
}

fn disk_spaces(disks: &Disks) -> Vec<DiskSpace> {
    disks
        .list()
        .iter()
        .map(|d| DiskSpace {
            mount_point: d.mount_point().to_owned(),
            total: d.total_space(),
            available: d.available_space(),
            device: d.name().to_string_lossy().into_owned(),
            file_system: d.file_system().to_string_lossy().into_owned(),
            removable: d.is_removable(),
        })
        .collect()
}

/// File systems that are not disks: memory, images, containers, firmware.
const NOT_DISKS: &[&str] = &[
    "tmpfs", "devtmpfs", "ramfs", "squashfs", "overlay", "efivarfs", "autofs", "nsfs",
];

/// Fixed disks for the agent status: no removable media, no pseudo file
/// systems, each device once (at its first mount point, e.g. `/` rather
/// than a bind mount or btrfs subvolume of it), ordered by mount point.
/// Used space is rounded (see [`protocol::DiskUsage::rounded`]).
pub fn fixed_disks(disks: &[DiskSpace]) -> Vec<protocol::DiskUsage> {
    let mut sorted: Vec<&DiskSpace> = disks
        .iter()
        .filter(|d| !d.removable && d.total > 0)
        .filter(|d| !NOT_DISKS.contains(&d.file_system.to_ascii_lowercase().as_str()))
        .collect();
    sorted.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    let mut seen = std::collections::HashSet::new();
    sorted
        .into_iter()
        // Windows volumes may have no label, or share one: key those by mount.
        .filter(|d| cfg!(windows) || d.device.is_empty() || seen.insert(d.device.clone()))
        .map(|d| {
            protocol::DiskUsage {
                name: d.mount_point.to_string_lossy().into_owned(),
                total_bytes: d.total,
                used_bytes: d.total.saturating_sub(d.available),
            }
            .rounded()
        })
        .collect()
}

/// Where the OS is installed: `C:\` (from `%SystemDrive%`) on Windows, `/` elsewhere.
pub fn system_root() -> PathBuf {
    #[cfg(windows)]
    {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        PathBuf::from(format!("{drive}\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/")
    }
}

/// `(used, total)` bytes of the disk mounted at `root`, falling back to the
/// largest disk if none is mounted exactly there.
pub fn system_disk_usage(disks: &[DiskSpace], root: &Path) -> Option<(u64, u64)> {
    let disk = disks
        .iter()
        .find(|d| mount_matches(&d.mount_point, root))
        .or_else(|| disks.iter().max_by_key(|d| d.total))?;
    Some((disk.total.saturating_sub(disk.available), disk.total))
}

fn mount_matches(mount: &Path, root: &Path) -> bool {
    // Windows drive letters are case-insensitive ("c:\" vs "C:\").
    mount
        .to_string_lossy()
        .eq_ignore_ascii_case(&root.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(mount: &str, total: u64, available: u64) -> DiskSpace {
        DiskSpace {
            mount_point: mount.into(),
            total,
            available,
            device: format!("dev{mount}"),
            file_system: "ext4".into(),
            removable: false,
        }
    }

    #[test]
    fn fixed_disks_skip_removable_pseudo_and_repeated_devices() {
        let disks = [
            disk("/home", 1000, 400),
            DiskSpace {
                file_system: "tmpfs".into(),
                ..disk("/tmp", 10, 10)
            },
            DiskSpace {
                removable: true,
                ..disk("/media/usb", 64, 60)
            },
            // A btrfs subvolume of the root file system.
            DiskSpace {
                device: "dev/".into(),
                ..disk("/var", 500, 200)
            },
            disk("/", 500, 200),
            disk("/empty", 0, 0),
        ];
        let names: Vec<(String, u64, u64)> = fixed_disks(&disks)
            .into_iter()
            .map(|d| (d.name, d.total_bytes, d.used_bytes))
            .collect();
        let expected: &[(&str, u64, u64)] = if cfg!(windows) {
            &[("/", 500, 300), ("/home", 1000, 600), ("/var", 500, 300)]
        } else {
            &[("/", 500, 300), ("/home", 1000, 600)]
        };
        let expected: Vec<(String, u64, u64)> = expected
            .iter()
            .map(|(n, t, u)| (n.to_string(), *t, *u))
            .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn this_machine_has_a_fixed_disk() {
        let disks = SystemTelemetry::new().disks();
        println!("disks: {disks:?}");
        assert!(!disks.is_empty());
        assert!(disks.iter().all(|d| d.used_bytes <= d.total_bytes));
    }

    #[test]
    fn picks_the_disk_mounted_at_the_system_root() {
        let disks = [
            disk("/data", 4000, 1000),
            disk("/", 500, 200),
            disk("/boot", 1, 1),
        ];
        assert_eq!(system_disk_usage(&disks, Path::new("/")), Some((300, 500)));
    }

    #[test]
    fn matches_windows_drive_letters_case_insensitively() {
        let disks = [disk("D:\\", 1000, 900), disk("c:\\", 256, 56)];
        assert_eq!(
            system_disk_usage(&disks, Path::new("C:\\")),
            Some((200, 256))
        );
    }

    #[test]
    fn falls_back_to_the_largest_disk() {
        let disks = [disk("/a", 10, 5), disk("/b", 30, 10)];
        assert_eq!(system_disk_usage(&disks, Path::new("/")), Some((20, 30)));
        assert_eq!(system_disk_usage(&[], Path::new("/")), None);
    }

    #[test]
    fn available_above_total_does_not_underflow() {
        assert_eq!(
            system_disk_usage(&[disk("/", 10, 20)], Path::new("/")),
            Some((0, 10))
        );
    }

    #[test]
    fn live_sample_is_plausible() {
        let t = SystemTelemetry::new().sample();
        assert!((0.0..=100.0).contains(&t.cpu_percent), "{t:?}");
        assert!(
            t.mem_total_bytes > 0 && t.mem_used_bytes <= t.mem_total_bytes,
            "{t:?}"
        );
        assert!(t.disk_used_bytes <= t.disk_total_bytes, "{t:?}");
        assert!(t.uptime_secs > 0, "{t:?}");
    }

    #[test]
    fn signed_in_users_are_named_once_each() {
        // Who is signed in depends on the machine; the list must be clean.
        let users = SystemTelemetry::new().signed_in_users();
        println!("signed in: {users:?}");
        assert!(users.iter().all(|u| !u.is_empty()), "{users:?}");
        let mut unique = users.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), users.len(), "{users:?}");
    }
}
