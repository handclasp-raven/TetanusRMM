//! Start a program on the signed-in user's desktop, for the viewer's
//! command buttons (see `protocol::launch`).
//!
//! Windows: the program runs in the console session **as the signed-in
//! user**, with their environment, never as SYSTEM, so a button cannot
//! hand the user (or the technician) more rights than the user has.
//! `cmd /c start` resolves the command like the Run dialog does: programs
//! on the PATH (`cmd`, `mstsc`), control panel applets (`ncpa.cpl`),
//! consoles (`services.msc`) and documents. It is not a security boundary:
//! the technician already controls this desktop.
//!
//! Elsewhere (development builds): `sh -c`, as the agent's own user.

use protocol::launch::{LaunchReply, LaunchRequest};

/// Start `request.command`; does not wait for it to exit.
pub async fn run(request: &LaunchRequest) -> LaunchReply {
    // Checked on the server too; never trust the other end.
    let command = match LaunchRequest::new(&request.command) {
        Ok(request) => request.command,
        Err(e) => return LaunchReply::Failed(e.to_string()),
    };
    tokio::task::spawn_blocking(move || start(&command))
        .await
        .unwrap_or_else(|e| LaunchReply::Failed(e.to_string()))
}

#[cfg(windows)]
fn start(command: &str) -> LaunchReply {
    use crate::win::process;

    let Some(session) = process::console_user_session() else {
        return LaunchReply::NoUser;
    };
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let cmd = std::path::Path::new(&system_root).join(r"System32\cmd.exe");
    // `/d`: no AutoRun commands. `start` opens its own window for console
    // programs (cmd itself runs without one), in the user's profile folder;
    // cmd expands %USERPROFILE% from the user's environment.
    let args = format!(r#"/d /c start "" /d "%USERPROFILE%" {command}"#);
    match process::spawn_in_session(session, &cmd, &args) {
        Ok(child) => {
            child.resume();
            LaunchReply::Started {
                user: crate::telemetry::session_user(session).unwrap_or_default(),
            }
        }
        Err(e) => LaunchReply::Failed(format!("cannot start it: {e}")),
    }
}

#[cfg(not(windows))]
fn start(command: &str) -> LaunchReply {
    use std::process::{Command, Stdio};

    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap it whenever it exits.
            std::thread::spawn(move || child.wait());
            LaunchReply::Started {
                user: std::env::var("USER").unwrap_or_default(),
            }
        }
        Err(e) => LaunchReply::Failed(format!("cannot start it: {e}")),
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starts_the_command_without_waiting() {
        let dir = std::env::temp_dir().join(format!("rmm-launch-{}", std::process::id()));
        let marker = dir.join("ran");
        std::fs::create_dir_all(&dir).unwrap();
        let request =
            LaunchRequest::new(&format!("sleep 0.2; touch '{}'", marker.display())).unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(run(&request).await, LaunchReply::Started { .. }));
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(marker.exists(), "the command ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn refuses_bad_commands() {
        let request = LaunchRequest {
            command: "  ".into(),
        };
        assert!(matches!(run(&request).await, LaunchReply::Failed(_)));
    }
}
