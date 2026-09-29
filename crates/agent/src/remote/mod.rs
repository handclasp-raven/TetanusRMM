//! Remote operations the server runs on this agent, each on its own
//! bidirectional stream that the server opens (see `protocol::StreamOpen`):
//!
//! - [`shell`]: interactive PowerShell on a pseudoconsole
//! - [`script`]: a script or command run without a terminal, one result
//! - [`transfer`]: file upload and download, chunked and hash-verified
//!
//! None of these involve the session helper or the user's desktop, and none
//! are gated by the consent policy, which governs remote *desktop* sessions.
//! The server checks the technician's role and audits every operation.

pub mod pty;
pub mod script;
pub mod shell;
pub mod transfer;

use protocol::{read_frame, write_frame, FrameError, StreamOpen};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, warn};

/// Serve one stream opened by the server, until the operation is over.
/// The caller finishes the send side afterwards.
pub async fn serve<W, R>(send: &mut W, recv: &mut R) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    let Some(open) = read_frame::<_, StreamOpen>(recv).await? else {
        return Ok(());
    };
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
    }
    .inspect_err(|e| warn!("remote operation stream failed: {e}"))
}
