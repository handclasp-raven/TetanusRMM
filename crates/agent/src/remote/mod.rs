//! Remote operations the server runs on this agent, each on its own
//! bidirectional stream that the server opens (see `protocol::StreamOpen`):
//!
//! - [`shell`]: interactive PowerShell on a pseudoconsole
//! - [`script`]: a script or command run without a terminal, one result
//! - [`transfer`]: file upload and download, chunked and hash-verified
//! - [`launch`]: start a program on the signed-in user's desktop
//!
//! Only [`launch`] touches the user's desktop (starting a program there as
//! the user, for the viewer's command buttons). None are gated by the
//! consent policy, which governs remote *desktop* sessions.
//! The server checks the technician's role and audits every operation.
//!
//! A quick assist client serves file transfer only ([`Allowed::FilesOnly`]):
//! the server would refuse the rest for it anyway, and so does it.

pub mod launch;
pub mod pty;
pub mod script;
pub mod shell;
pub mod transfer;

use protocol::{read_frame, write_frame, FrameError, StreamOpen};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, warn};

/// Which remote operations this agent serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Allowed {
    /// All of them (an installed agent).
    #[default]
    All,
    /// File upload and download only (a quick assist session).
    FilesOnly,
}

impl Allowed {
    fn permits(self, open: &StreamOpen) -> bool {
        match self {
            Allowed::All => true,
            Allowed::FilesOnly => {
                matches!(open, StreamOpen::Upload(_) | StreamOpen::Download { .. })
            }
        }
    }
}

/// What the server is told when an operation is not [`Allowed`].
const REFUSED: &str = "not available in a quick assist session";

/// Serve one stream opened by the server, until the operation is over.
/// The caller finishes the send side afterwards.
pub async fn serve<W, R>(allowed: Allowed, send: &mut W, recv: &mut R) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    let Some(open) = read_frame::<_, StreamOpen>(recv).await? else {
        return Ok(());
    };
    if !allowed.permits(&open) {
        warn!(?allowed, "remote operation refused");
        // Each kind of stream has its own way of saying no.
        return match open {
            StreamOpen::Shell { .. } => {
                write_frame(send, &protocol::shell::ShellOutput::Error(REFUSED.into())).await
            }
            StreamOpen::Script(_) => {
                write_frame(send, &protocol::script::ScriptReply::Failed(REFUSED.into())).await
            }
            StreamOpen::Launch(_) => {
                write_frame(send, &protocol::launch::LaunchReply::Failed(REFUSED.into())).await
            }
            StreamOpen::Upload(_) | StreamOpen::Download { .. } => Ok(()),
        };
    }
    match open {
        StreamOpen::Shell { size } => shell::serve(size, send, recv).await,
        StreamOpen::Script(request) => {
            info!(
                bytes = request.script.len(),
                timeout_secs = request.timeout_secs,
                "script requested"
            );
            let reply = script::run(&request).await;
            write_frame(send, &reply).await
        }
        StreamOpen::Upload(request) => transfer::upload(request, send, recv).await,
        StreamOpen::Download { path } => transfer::download(&path, send).await,
        StreamOpen::Launch(request) => {
            info!(command = %request.command, "launch requested");
            let reply = launch::run(&request).await;
            info!(?reply, "launch");
            write_frame(send, &reply).await
        }
    }
    .inspect_err(|e| warn!("remote operation stream failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::script::{ScriptReply, ScriptRequest};

    #[test]
    fn a_quick_assist_session_serves_files_only() {
        let script = StreamOpen::Script(ScriptRequest::new("whoami".into(), None).unwrap());
        let shell = StreamOpen::Shell {
            size: Default::default(),
        };
        let download = StreamOpen::Download {
            path: "C:\\x".into(),
        };
        for open in [&script, &shell, &download] {
            assert!(Allowed::All.permits(open));
        }
        assert!(Allowed::FilesOnly.permits(&download));
        assert!(!Allowed::FilesOnly.permits(&script));
        assert!(!Allowed::FilesOnly.permits(&shell));
    }

    #[tokio::test]
    async fn a_refused_script_is_answered_not_run() {
        let (mut server, mut agent) = tokio::io::duplex(64 * 1024);
        let request = StreamOpen::Script(ScriptRequest::new("exit 0".into(), None).unwrap());
        write_frame(&mut server, &request).await.unwrap();
        let (mut recv, mut send) = tokio::io::split(&mut agent);
        serve(Allowed::FilesOnly, &mut send, &mut recv)
            .await
            .unwrap();
        let reply: ScriptReply = read_frame(&mut server).await.unwrap().unwrap();
        assert_eq!(reply, ScriptReply::Failed(REFUSED.into()));
    }
}
