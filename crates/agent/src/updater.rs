//! Installing a verified update: rename-and-replace, relaunch, clean up.
//!
//! Windows will not let a running executable be overwritten or deleted, but it
//! does allow it to be *renamed*. So the swap is:
//!
//! 1. [`stage`]: write the verified binary to `agent.exe.new` beside the
//!    running one (same directory, so the renames below stay on one volume).
//! 2. [`swap`]: rename the running `agent.exe` to `agent.exe.old`, then
//!    rename `agent.exe.new` to `agent.exe`. If the second rename fails, the
//!    first is undone.
//! 3. [`relaunch`]: start the new `agent.exe` and exit this process.
//! 4. [`cleanup`], at the next start: delete `agent.exe.old`. On Windows this
//!    can fail for a moment while the old process is still exiting, so it
//!    retries briefly and otherwise leaves it for next time.
//!
//! The same steps are safe on Unix (where a plain rename over the running
//! binary would also work), so there is one code path with only the
//! platform-specific parts cfg-gated: file permissions and how to relaunch.
//! Once the agent runs as a Windows service (Phase 4), relaunch will become
//! "exit and let the service manager restart us".

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{info, warn};

/// The three paths involved in an update of one executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePaths {
    pub current: PathBuf,
    pub staged: PathBuf,
    pub old: PathBuf,
}

impl UpdatePaths {
    pub fn for_exe(exe: &Path) -> Self {
        let with_suffix = |suffix: &str| {
            let mut name: OsString = exe.as_os_str().to_owned();
            name.push(suffix);
            PathBuf::from(name)
        };
        Self {
            current: exe.to_owned(),
            staged: with_suffix(".new"),
            old: with_suffix(".old"),
        }
    }

    /// Paths for the running executable.
    pub fn for_current_exe() -> io::Result<Self> {
        Ok(Self::for_exe(&std::env::current_exe()?))
    }
}

/// Write the new binary next to the current one, flush it to disk, and make
/// it executable.
pub fn stage(paths: &UpdatePaths, binary: &[u8]) -> io::Result<()> {
    use std::io::Write;
    // One writable handle for write + flush: Windows' FlushFileBuffers
    // (behind sync_all) needs write access, unlike fsync on Unix.
    let mut file = std::fs::File::create(&paths.staged)?;
    file.write_all(binary)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&paths.staged, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Move the staged binary into place, keeping the old one as `.old`.
pub fn swap(paths: &UpdatePaths) -> io::Result<()> {
    swap_with(paths, |from, to| std::fs::rename(from, to))
}

/// [`swap`] with the rename operation injectable, so tests can make it fail.
fn swap_with(
    paths: &UpdatePaths,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    if !paths.staged.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no staged update to install",
        ));
    }
    // A leftover from an earlier update would block the rename on Windows.
    match std::fs::remove_file(&paths.old) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    rename(&paths.current, &paths.old)?;
    if let Err(e) = rename(&paths.staged, &paths.current) {
        // Put the running binary back so the agent still starts next time.
        if let Err(undo) = rename(&paths.old, &paths.current) {
            warn!("could not restore {}: {undo}", paths.current.display());
        }
        return Err(e);
    }
    info!(exe = %paths.current.display(), "update installed");
    Ok(())
}

/// Remove the previous binary left by [`swap`], if any.
pub fn cleanup(paths: &UpdatePaths) {
    const ATTEMPTS: u32 = 10;
    for attempt in 1..=ATTEMPTS {
        match std::fs::remove_file(&paths.old) {
            Ok(()) => {
                info!(old = %paths.old.display(), "removed previous binary");
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            // On Windows the old process may still be exiting.
            Err(e) if attempt < ATTEMPTS => {
                let _ = e;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => warn!(old = %paths.old.display(), "could not remove previous binary: {e}"),
        }
    }
}

/// Replace this process with the (new) binary at `exe`, passing `args`.
/// Only returns on failure.
#[cfg(unix)]
pub fn relaunch(exe: &Path, args: &[OsString]) -> io::Error {
    use std::os::unix::process::CommandExt;
    // exec keeps the PID, so a supervisor sees one continuous process.
    std::process::Command::new(exe).args(args).exec()
}

/// Start the (new) binary at `exe` with `args`, then exit this process.
/// Only returns on failure.
#[cfg(windows)]
pub fn relaunch(exe: &Path, args: &[OsString]) -> io::Error {
    use std::os::windows::process::CommandExt;
    // Detach so the child survives our exit and does not share our console.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    match std::process::Command::new(exe)
        .args(args)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, UpdatePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::for_exe(&dir.path().join("agent.exe"));
        std::fs::write(&paths.current, b"old version").unwrap();
        (dir, paths)
    }

    #[test]
    fn paths_sit_beside_the_executable() {
        let paths = UpdatePaths::for_exe(Path::new("/opt/rmm/agent.exe"));
        assert_eq!(paths.staged, Path::new("/opt/rmm/agent.exe.new"));
        assert_eq!(paths.old, Path::new("/opt/rmm/agent.exe.old"));
    }

    #[test]
    fn stage_swap_cleanup() {
        let (_dir, paths) = setup();
        stage(&paths, b"new version").unwrap();
        swap(&paths).unwrap();
        assert_eq!(std::fs::read(&paths.current).unwrap(), b"new version");
        assert_eq!(std::fs::read(&paths.old).unwrap(), b"old version");
        assert!(!paths.staged.exists());

        cleanup(&paths);
        assert!(!paths.old.exists());
        cleanup(&paths); // nothing left: no-op
    }

    #[test]
    fn swap_without_a_staged_binary_changes_nothing() {
        let (_dir, paths) = setup();
        assert!(swap(&paths).is_err());
        assert_eq!(std::fs::read(&paths.current).unwrap(), b"old version");
        assert!(!paths.old.exists());
    }

    #[test]
    fn swap_replaces_a_leftover_old_binary() {
        let (_dir, paths) = setup();
        std::fs::write(&paths.old, b"ancient").unwrap();
        stage(&paths, b"new version").unwrap();
        swap(&paths).unwrap();
        assert_eq!(std::fs::read(&paths.old).unwrap(), b"old version");
    }

    #[test]
    fn swap_rolls_back_if_the_new_binary_cannot_be_moved_in() {
        let (_dir, paths) = setup();
        stage(&paths, b"new version").unwrap();
        let fail_staged_move = |from: &Path, to: &Path| {
            if from == paths.staged {
                Err(io::Error::other("injected failure"))
            } else {
                std::fs::rename(from, to)
            }
        };
        assert!(swap_with(&paths, fail_staged_move).is_err());
        assert_eq!(std::fs::read(&paths.current).unwrap(), b"old version");
        assert!(!paths.old.exists());
        assert!(paths.staged.exists(), "staged update is kept for a retry");
    }

    #[cfg(unix)]
    #[test]
    fn staged_binary_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, paths) = setup();
        stage(&paths, b"#!/bin/sh\n").unwrap();
        let mode = std::fs::metadata(&paths.staged)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}
