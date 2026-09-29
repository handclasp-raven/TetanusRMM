//! Non-interactive script runner.
//!
//! The server opens a bidirectional stream with
//! [`crate::StreamOpen::Script`], finishes its side, and reads exactly one
//! [`ScriptReply`]. The agent runs the script without a terminal (PowerShell
//! on Windows, `sh` on the development fallback), captures stdout and stderr
//! up to [`MAX_OUTPUT_BYTES`] each, and replies once the process exits or
//! the timeout kills it.

use serde::{Deserialize, Serialize};

/// Largest script accepted, in bytes of UTF-8.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;

/// Output kept per stream (stdout, stderr). Anything beyond is discarded and
/// the result is marked truncated.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub const DEFAULT_TIMEOUT_SECS: u32 = 300;
pub const MAX_TIMEOUT_SECS: u32 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptRequest {
    /// Script text, or a single command (a one-line script).
    pub script: String,
    /// Kill the script (and its children) after this long.
    pub timeout_secs: u32,
}

/// What a finished script produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptResult {
    /// `None` if the process was killed (timeout) or had no exit code.
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub duration_ms: u64,
}

impl ScriptResult {
    /// Exited on its own with code 0.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// Agent to server: the only frame on a script stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScriptReply {
    Completed(ScriptResult),
    /// The script could not be run at all (e.g. the interpreter is missing).
    Failed(String),
}

/// Keeps the first `cap` bytes of a stream and counts the rest.
#[derive(Debug, Clone)]
pub struct CappedOutput {
    cap: usize,
    buf: Vec<u8>,
    total: u64,
}

impl CappedOutput {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: Vec::new(),
            total: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let room = self.cap.saturating_sub(self.buf.len());
        self.buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    pub fn truncated(&self) -> bool {
        self.total > self.buf.len() as u64
    }

    /// Total bytes seen, kept or not.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// `(kept bytes, truncated)`
    pub fn finish(self) -> (Vec<u8>, bool) {
        let truncated = self.truncated();
        (self.buf, truncated)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error("script is empty")]
    Empty,
    #[error("script is larger than {MAX_SCRIPT_BYTES} bytes")]
    TooLarge,
    #[error("timeout_secs must be between 1 and {MAX_TIMEOUT_SECS}")]
    BadTimeout,
}

impl ScriptRequest {
    /// Validate a request; `timeout_secs` defaults to [`DEFAULT_TIMEOUT_SECS`].
    pub fn new(script: String, timeout_secs: Option<u32>) -> Result<Self, RequestError> {
        if script.trim().is_empty() {
            return Err(RequestError::Empty);
        }
        if script.len() > MAX_SCRIPT_BYTES {
            return Err(RequestError::TooLarge);
        }
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(1..=MAX_TIMEOUT_SECS).contains(&timeout_secs) {
            return Err(RequestError::BadTimeout);
        }
        Ok(Self {
            script,
            timeout_secs,
        })
    }

    /// Short description for the audit log: the first line, capped at
    /// `max_chars`, with "…" if anything was left out.
    pub fn summary(&self, max_chars: usize) -> String {
        let trimmed = self.script.trim();
        let first = trimmed.lines().next().unwrap_or("");
        let mut out: String = first.chars().take(max_chars).collect();
        if out.len() < trimmed.len() {
            out.push('…');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_output_keeps_the_head_and_counts_the_rest() {
        let mut out = CappedOutput::new(5);
        out.push(b"abc");
        assert!(!out.truncated());
        out.push(b"defgh");
        out.push(b"ij");
        assert!(out.truncated());
        assert_eq!(out.total(), 10);
        assert_eq!(out.finish(), (b"abcde".to_vec(), true));

        let mut exact = CappedOutput::new(3);
        exact.push(b"abc");
        exact.push(b"");
        assert_eq!(exact.finish(), (b"abc".to_vec(), false));
    }

    #[test]
    fn requests_are_validated() {
        assert_eq!(
            ScriptRequest::new("hostname".into(), None).unwrap(),
            ScriptRequest {
                script: "hostname".into(),
                timeout_secs: DEFAULT_TIMEOUT_SECS
            }
        );
        assert_eq!(
            ScriptRequest::new("  \n".into(), None),
            Err(RequestError::Empty)
        );
        assert_eq!(
            ScriptRequest::new("x".repeat(MAX_SCRIPT_BYTES + 1), None),
            Err(RequestError::TooLarge)
        );
        assert_eq!(
            ScriptRequest::new("x".into(), Some(0)),
            Err(RequestError::BadTimeout)
        );
        assert_eq!(
            ScriptRequest::new("x".into(), Some(MAX_TIMEOUT_SECS + 1)),
            Err(RequestError::BadTimeout)
        );
        assert!(ScriptRequest::new("x".into(), Some(MAX_TIMEOUT_SECS)).is_ok());
    }

    #[test]
    fn summary_is_the_first_line_and_marks_omissions() {
        let req = |s: &str| ScriptRequest::new(s.into(), None).unwrap();
        assert_eq!(req("Get-Service").summary(80), "Get-Service");
        assert_eq!(
            req("\n  Get-Process\nStop-Computer\n").summary(80),
            "Get-Process…"
        );
        assert_eq!(req("abcdefgh").summary(3), "abc…");
        // Multi-byte characters are never split.
        assert_eq!(req("ééé").summary(2), "éé…");
    }

    #[test]
    fn success_means_exit_zero_without_timeout() {
        let mut r = ScriptResult {
            exit_code: Some(0),
            stdout: vec![],
            stderr: vec![],
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
            duration_ms: 1,
        };
        assert!(r.succeeded());
        r.exit_code = Some(1);
        assert!(!r.succeeded());
        r.exit_code = None;
        r.timed_out = true;
        assert!(!r.succeeded());
    }

    #[test]
    fn reply_round_trips() {
        let reply = ScriptReply::Completed(ScriptResult {
            exit_code: Some(-1),
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
            stdout_truncated: true,
            stderr_truncated: false,
            timed_out: false,
            duration_ms: 42,
        });
        let bytes = postcard::to_stdvec(&reply).unwrap();
        assert_eq!(postcard::from_bytes::<ScriptReply>(&bytes).unwrap(), reply);
    }
}
