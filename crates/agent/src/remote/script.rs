//! Non-interactive scripts (see `protocol::script`): no terminal, output
//! captured, one result.
//!
//! - **Windows:** the script is written to a temporary `.ps1` and run by
//!   Windows PowerShell with `-NonInteractive -ExecutionPolicy Bypass`,
//!   output forced to UTF-8. Under the service it runs as SYSTEM.
//! - **Unix (development fallback):** `/bin/sh -c <script>`.
//!
//! On timeout the whole process tree is killed and the result is marked
//! `timed_out` with no exit code.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::script::{CappedOutput, ScriptReply, ScriptRequest, ScriptResult, MAX_OUTPUT_BYTES};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tracing::{info, warn};

/// After the script exits, how long to keep reading output that a program it
/// started in the background may still be holding open.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// Run `request` to completion (or timeout).
pub async fn run(request: &ScriptRequest) -> ScriptReply {
    let started = Instant::now();
    let prepared = match Prepared::new(&request.script) {
        Ok(p) => p,
        Err(e) => return ScriptReply::Failed(format!("preparing the script: {e}")),
    };
    let mut child = match prepared.command().spawn() {
        Ok(child) => child,
        Err(e) => return ScriptReply::Failed(format!("starting the interpreter: {e}")),
    };
    let pid = child.id();
    info!(?pid, bytes = request.script.len(), "script started");

    let stdout = capture(child.stdout.take());
    let stderr = capture(child.stderr.take());
    let timeout = Duration::from_secs(request.timeout_secs.into());
    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => {
            warn!("waiting for the script failed: {e}");
            kill_tree(&mut child).await;
            (None, false)
        }
        Err(_) => {
            warn!(
                ?pid,
                timeout_secs = request.timeout_secs,
                "script timed out; killing it"
            );
            kill_tree(&mut child).await;
            (None, true)
        }
    };
    let (stdout, stdout_truncated) = stdout.finish().await;
    let (stderr, stderr_truncated) = stderr.finish().await;
    drop(prepared);

    let duration_ms = started.elapsed().as_millis() as u64;
    info!(?pid, ?exit_code, timed_out, duration_ms, "script finished");
    ScriptReply::Completed(ScriptResult {
        exit_code,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        timed_out,
        duration_ms,
    })
}

/// Output being read from one pipe into a capped buffer.
struct Capture {
    buffer: Arc<Mutex<CappedOutput>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

fn capture<R: AsyncRead + Unpin + Send + 'static>(pipe: Option<R>) -> Capture {
    let buffer = Arc::new(Mutex::new(CappedOutput::new(MAX_OUTPUT_BYTES)));
    let task = pipe.map(|mut pipe| {
        let buffer = buffer.clone();
        tokio::spawn(async move {
            let mut chunk = vec![0u8; 16 * 1024];
            // Keep reading past the cap so the script never blocks on a full pipe.
            while let Ok(n @ 1..) = pipe.read(&mut chunk).await {
                buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(&chunk[..n]);
            }
        })
    });
    Capture { buffer, task }
}

impl Capture {
    /// Wait briefly for the rest of the output, then take what arrived.
    async fn finish(mut self) -> (Vec<u8>, bool) {
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(DRAIN_AFTER_EXIT, task).await.is_err() {
                abort.abort();
            }
        }
        let buffer = std::mem::replace(
            &mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()),
            CappedOutput::new(0),
        );
        buffer.finish()
    }
}

/// Kill the script and everything it started.
async fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        // taskkill /T walks the process tree, which Child::kill does not.
        let _ = Command::new(system32("taskkill.exe"))
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .await;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The script leads its own process group (see `Prepared::command`).
        // SAFETY: signals the script's process group.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.kill().await;
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn system32(exe: &str) -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::Path::new(&root).join("System32").join(exe)
}

/// A script ready to run; on Windows, owns its temporary file.
struct Prepared {
    #[cfg(windows)]
    file: std::path::PathBuf,
    #[cfg(not(windows))]
    script: String,
}

#[cfg(windows)]
impl Prepared {
    fn new(script: &str) -> std::io::Result<Self> {
        use std::io::Write;
        // A random name, created exclusively, so nothing else can have
        // planted a file there first.
        let file = std::env::temp_dir().join(format!("rmm-script-{}.ps1", random_hex()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)?;
        // The BOM makes Windows PowerShell 5.1 read it as UTF-8.
        f.write_all(b"\xEF\xBB\xBF")?;
        f.write_all(script.as_bytes())?;
        Ok(Self { file })
    }

    fn command(&self) -> Command {
        use crate::win::conpty::powershell_path;
        // Quote for a single-quoted PowerShell string.
        let path = self.file.display().to_string().replace('\'', "''");
        // UTF-8 output whatever the console code page, and the script's own
        // `exit N` (or last native exit code) as ours.
        let wrapper = format!(
            "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); \
             & '{path}'; exit $LASTEXITCODE"
        );
        let mut command = Command::new(powershell_path());
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &wrapper,
            ])
            .creation_flags(CREATE_NO_WINDOW);
        configure(&mut command);
        command
    }
}

#[cfg(windows)]
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

#[cfg(windows)]
fn random_hex() -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG available");
    hex::encode(bytes)
}

#[cfg(not(windows))]
impl Prepared {
    fn new(script: &str) -> std::io::Result<Self> {
        Ok(Self {
            script: script.to_owned(),
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(&self.script);
        #[cfg(unix)]
        command.process_group(0);
        configure(&mut command);
        command
    }
}

fn configure(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn request(script: &str, timeout_secs: u32) -> ScriptRequest {
        ScriptRequest::new(script.into(), Some(timeout_secs)).unwrap()
    }

    fn completed(reply: ScriptReply) -> ScriptResult {
        match reply {
            ScriptReply::Completed(r) => r,
            ScriptReply::Failed(e) => panic!("failed: {e}"),
        }
    }

    #[tokio::test]
    async fn captures_stdout_stderr_and_exit_code() {
        let r = completed(run(&request("echo out; echo err >&2; exit 4", 30)).await);
        assert_eq!(r.stdout, b"out\n");
        assert_eq!(r.stderr, b"err\n");
        assert_eq!(r.exit_code, Some(4));
        assert!(!r.timed_out && !r.stdout_truncated && !r.stderr_truncated);
    }

    #[tokio::test]
    async fn output_beyond_the_cap_is_truncated_without_blocking_the_script() {
        // Twice the cap: the script only finishes if we keep draining.
        let script = format!("head -c {} /dev/zero; echo done >&2", MAX_OUTPUT_BYTES * 2);
        let r = completed(run(&request(&script, 30)).await);
        assert_eq!(r.stdout.len(), MAX_OUTPUT_BYTES);
        assert!(r.stdout_truncated);
        assert_eq!(r.stderr, b"done\n");
        assert_eq!(r.exit_code, Some(0));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_tree() {
        let started = Instant::now();
        // The background sleep holds stdout open; it must die too.
        let r = completed(run(&request("sleep 30 & echo started; wait", 1)).await);
        assert!(r.timed_out);
        assert_eq!(r.exit_code, None);
        assert_eq!(r.stdout, b"started\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn completed(script: &str, timeout_secs: u32) -> ScriptResult {
        let request = ScriptRequest::new(script.into(), Some(timeout_secs)).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        match rt.block_on(run(&request)) {
            ScriptReply::Completed(r) => r,
            ScriptReply::Failed(e) => panic!("failed: {e}"),
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).expect("PowerShell output is UTF-8")
    }

    #[test]
    fn powershell_output_streams_and_exit_code() {
        let r = completed(
            "Write-Output 'out'; [Console]::Error.WriteLine('err'); exit 4",
            60,
        );
        assert_eq!(text(&r.stdout).trim_end(), "out");
        assert_eq!(text(&r.stderr).trim_end(), "err");
        assert_eq!(r.exit_code, Some(4));
        assert!(!r.timed_out);
    }

    #[test]
    fn output_is_utf8_whatever_the_console_code_page() {
        let r = completed("Write-Output 'h\u{e9}llo \u{2713}'", 60);
        assert_eq!(text(&r.stdout).trim_end(), "h\u{e9}llo \u{2713}");
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn a_thrown_error_fails_the_script() {
        let r = completed("Write-Output before\nthrow 'boom'\nWrite-Output after", 60);
        assert_eq!(text(&r.stdout).trim_end(), "before");
        assert!(text(&r.stderr).contains("boom"), "{}", text(&r.stderr));
        assert_eq!(r.exit_code, Some(1));
    }

    #[test]
    fn timeout_kills_powershell_and_its_children() {
        let started = Instant::now();
        let r = completed(
            "Write-Output started; & ping.exe -n 60 127.0.0.1 | Out-Null",
            3,
        );
        assert!(r.timed_out);
        assert_eq!(r.exit_code, None);
        assert_eq!(text(&r.stdout).trim_end(), "started");
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn the_temporary_script_file_is_removed() {
        let r = completed("Write-Output $PSCommandPath", 60);
        let path = text(&r.stdout).trim_end().to_owned();
        assert!(
            path.contains("rmm-script-") && path.ends_with(".ps1"),
            "{path}"
        );
        assert!(!std::path::Path::new(&path).exists(), "{path} left behind");
    }
}
