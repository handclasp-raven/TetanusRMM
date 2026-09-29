//! Health telemetry (CPU, memory, system disk, uptime) sent on each heartbeat.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::Telemetry;
use sysinfo::{Disks, System};

/// Something that can produce a telemetry sample. Sampling may block briefly
/// (it reads OS counters), so callers run it on the blocking pool.
pub trait TelemetrySource: Send + Sync {
    fn sample(&self) -> Telemetry;
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
        let disk_list: Vec<DiskSpace> = disks
            .list()
            .iter()
            .map(|d| DiskSpace {
                mount_point: d.mount_point().to_owned(),
                total: d.total_space(),
                available: d.available_space(),
            })
            .collect();
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
}

/// One mounted disk, as far as telemetry cares.
#[derive(Debug, Clone)]
pub struct DiskSpace {
    pub mount_point: PathBuf,
    pub total: u64,
    pub available: u64,
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
        }
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
}
