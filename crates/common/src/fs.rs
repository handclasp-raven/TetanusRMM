//! File helpers.

use std::fs;
use std::path::Path;

/// Write `contents` to `path`, replacing it. With `secret`, the file is
/// created with mode 0600 on Unix. (On Windows, files inherit the directory
/// ACL; secrets there are protected with DPAPI instead.)
pub fn write_file(path: &Path, contents: &[u8], secret: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mode = if secret { 0o600 } else { 0o644 };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)?;
        // `mode` only applies on creation; tighten an existing file too.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(contents)?;
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = secret;
        fs::write(path, contents)
    }
}
