//! Where the agent keeps its files.
//!
//! Windows:
//! - `%ProgramFiles%\RMM\rmm-agent.exe`: the installed binary (the service
//!   runs it; the updater replaces it in place).
//! - `%ProgramData%\RMM\agent\`: state directory (credential, pending
//!   enrollment, `agent.log`), readable only by SYSTEM and Administrators.
//! - `%LOCALAPPDATA%\RMM\helper.log`: the session helper's log, per user.
//! - `%ProgramData%\RMM\agent\input-helper.log`: the input helper's log
//!   (it runs as SYSTEM, so it logs next to the service).
//!
//! Elsewhere (development): `./agent-state`.

use std::path::PathBuf;

/// Name of the installed executable.
pub const EXE_NAME: &str = "rmm-agent.exe";

/// Default state directory.
pub fn default_state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        env_dir("ProgramData", "C:\\ProgramData")
            .join("RMM")
            .join("agent")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("agent-state")
    }
}

/// Where `service install` puts the binary.
#[cfg(windows)]
pub fn install_dir() -> PathBuf {
    env_dir("ProgramFiles", "C:\\Program Files").join("RMM")
}

/// Log file for the session helper (runs as the logged-on user).
#[cfg(windows)]
pub fn helper_log() -> PathBuf {
    env_dir("LOCALAPPDATA", "C:\\Users\\Public")
        .join("RMM")
        .join("helper.log")
}

/// Log file for the input helper (runs as SYSTEM in the user's session).
#[cfg(windows)]
pub fn input_helper_log() -> PathBuf {
    default_state_dir().join("input-helper.log")
}

#[cfg(windows)]
fn env_dir(var: &str, fallback: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_else(|| fallback.into()))
}
