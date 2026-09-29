//! File upload and download through the server (see `protocol::transfer`).
//!
//! The server never stores a file. An upload's HTTP body is cut into chunks
//! and forwarded to the agent as it arrives; a download's chunks become the
//! HTTP response body as they arrive. Either way only a chunk or two is in
//! memory, whatever the file's size, and QUIC flow control plus the HTTP
//! connection carry backpressure end to end.
//!
//! Integrity: the client states an upload's SHA-256 up front, and the agent
//! checks it before moving the file into place. For downloads the agent
//! hashes the file first and sends it in a manifest (returned as the
//! `x-content-sha256` header); the server checks the bytes against it as
//! they pass, and aborts the response on a mismatch so the client never
//! mistakes a bad copy for a good one.
//!
//! Every attempt that reaches an agent is audited as `file.upload` or
//! `file.download` with who, which agent, the path, the bytes moved and
//! how it ended (including a client that disconnects part-way).

use std::io;

use axum::body::Bytes;
use futures_util::{Stream, StreamExt};
use protocol::transfer::{
    ChunkReceiver, FileManifest, Rechunker, Sha256, TransferMsg, UploadRequest, CHUNK_SIZE,
};
use protocol::{read_frame, write_frame, StreamOpen};
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use super::RemoteError;
use crate::audit::{self, Action, NewEntry};
use crate::relay::{AgentLink, Hub};
use crate::users::{Capability, User};

/// A finished upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Uploaded {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

pub struct UploadParams {
    pub agent_id: String,
    pub path: String,
    pub overwrite: bool,
    /// From `Content-Length`.
    pub size: u64,
    /// From `x-content-sha256`.
    pub sha256: Sha256,
}

/// Parse a hex SHA-256.
pub fn parse_sha256(hex_digest: &str) -> Option<Sha256> {
    hex::decode(hex_digest.trim()).ok()?.try_into().ok()
}

fn check_path(path: &str) -> Result<(), RemoteError> {
    if path.trim().is_empty() || path.contains('\0') {
        return Err(RemoteError::BadRequest("path is required".into()));
    }
    Ok(())
}

/// A transfer's audit row.
struct Record<'a> {
    user: &'a str,
    action: Action,
    agent_id: &'a str,
    path: &'a str,
    /// Bytes that passed through the server.
    bytes: u64,
    /// The announced size, once known.
    size: Option<u64>,
}

/// Record how a transfer ended.
async fn audit_transfer(
    pool: &PgPool,
    record: Record<'_>,
    result: Result<&Sha256, &RemoteError>,
) -> sqlx::Result<()> {
    let Record {
        user,
        action,
        agent_id,
        path,
        bytes,
        size,
    } = record;
    let direction = if action == Action::FileUpload {
        "upload"
    } else {
        "download"
    };
    crate::metrics::get()
        .file_bytes
        .get_or_create(&crate::metrics::DirectionLabels { direction })
        .inc_by(bytes);
    let mut detail = json!({ "path": path, "bytes": bytes, "size": size });
    match result {
        Ok(sha256) => {
            detail["status"] = "completed".into();
            detail["sha256"] = hex::encode(sha256).into();
        }
        Err(e) => {
            detail["status"] = "failed".into();
            detail["error"] = e.to_string().into();
        }
    }
    audit::append_now(
        pool,
        NewEntry::new(user, action).target(agent_id).detail(detail),
    )
    .await
    .map(drop)
}

/// Upload `body` (exactly `params.size` bytes) to `params.path` on the agent.
pub async fn upload<S, E>(
    pool: &PgPool,
    hub: &Hub,
    user: &User,
    params: UploadParams,
    body: S,
) -> Result<Uploaded, RemoteError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let agents = [params.agent_id.clone()];
    super::authorize(pool, user, Capability::FileTransfer, &agents).await?;
    check_path(&params.path)?;
    let link = super::agent(hub, &params.agent_id)?;

    let mut sent = 0;
    let result = send_upload(&link, &params, body, &mut sent).await;
    let record = Record {
        user: &user.username,
        action: Action::FileUpload,
        agent_id: &params.agent_id,
        path: &params.path,
        bytes: sent,
        size: Some(params.size),
    };
    audit_transfer(pool, record, result.as_ref()).await?;
    let sha256 = result?;
    info!(user = %user.username, agent_id = %params.agent_id, path = %params.path, size = params.size, "file uploaded");
    Ok(Uploaded {
        path: params.path,
        size: params.size,
        sha256: hex::encode(sha256),
    })
}

async fn send_upload<S, E>(
    link: &AgentLink,
    params: &UploadParams,
    mut body: S,
    sent: &mut u64,
) -> Result<Sha256, RemoteError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let (mut send, mut recv) = super::open_stream(link).await?;
    let request = UploadRequest {
        manifest: FileManifest {
            path: params.path.clone(),
            size: params.size,
            sha256: params.sha256,
        },
        overwrite: params.overwrite,
    };
    write_frame(&mut send, &StreamOpen::Upload(request)).await?;
    match read_frame::<_, TransferMsg>(&mut recv).await? {
        Some(TransferMsg::Ready) => {}
        other => return Err(unexpected(other)),
    }

    // Forward the body. If the agent stops reading (it failed), writing
    // fails and its reason is read below.
    let mut chunker = Rechunker::new(CHUNK_SIZE);
    let forwarded: Result<(), RemoteError> = async {
        while let Some(piece) = body.next().await {
            let piece = piece
                .map_err(|e| RemoteError::BadRequest(format!("reading the request body: {e}")))?;
            if *sent + piece.len() as u64 > params.size {
                return Err(RemoteError::BadRequest(
                    "request body is longer than Content-Length".into(),
                ));
            }
            *sent += piece.len() as u64;
            for chunk in chunker.push(&piece) {
                write_frame(&mut send, &chunk).await?;
            }
        }
        if let Some(chunk) = chunker.flush() {
            write_frame(&mut send, &chunk).await?;
        }
        if *sent != params.size {
            return Err(RemoteError::BadRequest(
                "request body is shorter than Content-Length".into(),
            ));
        }
        Ok(())
    }
    .await;
    if let Err(e @ RemoteError::BadRequest(_)) = forwarded {
        // The agent discards the partial file when the stream is reset.
        let _ = send.reset(0);
        return Err(e);
    }
    let _ = send.shutdown().await;

    match read_frame::<_, TransferMsg>(&mut recv).await {
        Ok(Some(TransferMsg::Complete { size, sha256 }))
            if size == params.size && sha256 == params.sha256 =>
        {
            Ok(sha256)
        }
        Ok(other) => Err(unexpected(other)),
        // No reply: report why forwarding stopped, if it did.
        Err(e) => Err(forwarded.err().unwrap_or_else(|| e.into())),
    }
}

/// The agent's error, or a protocol violation.
fn unexpected(msg: Option<TransferMsg>) -> RemoteError {
    match msg {
        Some(TransferMsg::Error { kind, message }) => RemoteError::Agent { kind, message },
        Some(_) => RemoteError::Transport("unexpected message from the agent".into()),
        None => RemoteError::Transport("the agent closed the stream".into()),
    }
}

/// A download in progress: its manifest and its body.
pub struct Download {
    pub manifest: FileManifest,
    pub body: std::pin::Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>,
}

/// Start downloading `path` from the agent. The body verifies each chunk
/// and the final hash, and the transfer is audited when the body ends or is
/// dropped.
pub async fn download(
    pool: &PgPool,
    hub: &Hub,
    user: &User,
    agent_id: &str,
    path: &str,
) -> Result<Download, RemoteError> {
    let agents = [agent_id.to_owned()];
    super::authorize(pool, user, Capability::FileTransfer, &agents).await?;
    check_path(path)?;
    let link = super::agent(hub, agent_id)?;

    let (manifest, recv) = match start_download(&link, path).await {
        Ok(started) => started,
        Err(e) => {
            let record = Record {
                user: &user.username,
                action: Action::FileDownload,
                agent_id,
                path,
                bytes: 0,
                size: None,
            };
            audit_transfer(pool, record, Err(&e)).await?;
            return Err(e);
        }
    };
    info!(user = %user.username, agent_id, path, size = manifest.size, "download started");
    let audit = DownloadAudit {
        pool: pool.clone(),
        user: user.username.clone(),
        agent_id: agent_id.to_owned(),
        path: path.to_owned(),
        size: Some(manifest.size),
        bytes: 0,
        result: None,
    };
    let body = download_body(recv, &manifest, audit);
    Ok(Download {
        manifest,
        body: Box::pin(body),
    })
}

async fn start_download(
    link: &AgentLink,
    path: &str,
) -> Result<(FileManifest, transport::RecvStream), RemoteError> {
    let (mut send, mut recv) = super::open_stream(link).await?;
    write_frame(
        &mut send,
        &StreamOpen::Download {
            path: path.to_owned(),
        },
    )
    .await?;
    let _ = send.shutdown().await;
    match read_frame::<_, TransferMsg>(&mut recv).await? {
        Some(TransferMsg::Manifest(manifest)) => Ok((manifest, recv)),
        other => Err(unexpected(other)),
    }
}

struct BodyState<R> {
    recv: R,
    /// `None` once the transfer has ended, well or badly.
    receiver: Option<ChunkReceiver>,
    audit: DownloadAudit,
}

/// The chunks on `recv` as a verified byte stream. The final chunk is only
/// released once the whole file matches the manifest's hash; on any
/// integrity failure the stream ends with an error instead, so the HTTP
/// response is cut short.
pub(crate) fn download_body<R>(
    recv: R,
    manifest: &FileManifest,
    mut audit: DownloadAudit,
) -> impl Stream<Item = io::Result<Bytes>> + Send
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut receiver = Some(ChunkReceiver::for_manifest(manifest));
    let mut early_failure = None;
    if manifest.size == 0 {
        // No chunks will come (and an empty body may never be polled).
        match receiver.take().map(ChunkReceiver::finish) {
            Some(Ok(sha256)) => audit.result = Some(Ok(sha256)),
            Some(Err(e)) => early_failure = Some(e.to_string()),
            None => {}
        }
    }
    let state = BodyState {
        recv,
        receiver,
        audit,
    };
    futures_util::stream::unfold(
        (state, early_failure),
        |(mut state, early_failure)| async move {
            let failure = match early_failure {
                Some(failure) => failure,
                None => {
                    let receiver = state.receiver.as_mut()?;
                    match read_frame::<_, TransferMsg>(&mut state.recv).await {
                        Ok(Some(TransferMsg::Chunk { offset, data })) => {
                            match receiver.accept(offset, &data) {
                                Ok(()) if !receiver.is_complete() => {
                                    state.audit.bytes += data.len() as u64;
                                    return Some((Ok(Bytes::from(data)), (state, None)));
                                }
                                // The last chunk: verify the whole file first.
                                Ok(()) => match state.receiver.take()?.finish() {
                                    Ok(sha256) => {
                                        state.audit.bytes += data.len() as u64;
                                        state.audit.result = Some(Ok(sha256));
                                        return Some((Ok(Bytes::from(data)), (state, None)));
                                    }
                                    Err(e) => e.to_string(),
                                },
                                Err(e) => e.to_string(),
                            }
                        }
                        Ok(Some(TransferMsg::Error { message, .. })) => message,
                        Ok(Some(_)) => "unexpected message from the agent".into(),
                        Ok(None) => "the agent closed the stream early".into(),
                        Err(e) => e.to_string(),
                    }
                }
            };
            state.receiver = None;
            let error = state.audit.fail(failure);
            Some((Err(error), (state, None)))
        },
    )
}

/// Audits a download when its body is finished or dropped.
pub(crate) struct DownloadAudit {
    pool: PgPool,
    user: String,
    agent_id: String,
    path: String,
    size: Option<u64>,
    bytes: u64,
    /// `None` until it ends; still `None` when dropped means the client left.
    result: Option<Result<Sha256, RemoteError>>,
}

impl DownloadAudit {
    fn fail(&mut self, reason: String) -> io::Error {
        warn!(path = %self.path, "download failed: {reason}");
        self.result = Some(Err(RemoteError::Transport(reason.clone())));
        io::Error::other(reason)
    }
}

impl Drop for DownloadAudit {
    fn drop(&mut self) {
        let result = self.result.take().unwrap_or_else(|| {
            Err(RemoteError::Transport(
                "the client disconnected before the end".into(),
            ))
        });
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (pool, user, agent_id, path) = (
            self.pool.clone(),
            std::mem::take(&mut self.user),
            std::mem::take(&mut self.agent_id),
            std::mem::take(&mut self.path),
        );
        let (bytes, size) = (self.bytes, self.size);
        runtime.spawn(async move {
            if let Ok(sha256) = &result {
                info!(%agent_id, %path, bytes, sha256 = %hex::encode(sha256), "download verified");
            }
            let record = Record {
                user: &user,
                action: Action::FileDownload,
                agent_id: &agent_id,
                path: &path,
                bytes,
                size,
            };
            if let Err(e) = audit_transfer(&pool, record, result.as_ref()).await {
                warn!("auditing a download failed: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::transfer::sha256;

    fn audit() -> DownloadAudit {
        DownloadAudit {
            // Never connects: the audit write fails quietly in these tests.
            pool: PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/none").unwrap(),
            user: "u".into(),
            agent_id: "a".into(),
            path: "/f".into(),
            size: None,
            bytes: 0,
            result: None,
        }
    }

    /// Agent frames for `bytes` in chunks of `chunk`, as the body sees them.
    async fn body_of(manifest: &FileManifest, frames: Vec<TransferMsg>) -> Vec<io::Result<Bytes>> {
        let mut wire = Vec::new();
        for f in &frames {
            write_frame(&mut wire, f).await.unwrap();
        }
        download_body(std::io::Cursor::new(wire), manifest, audit())
            .collect()
            .await
    }

    fn chunks(bytes: &[u8], size: usize) -> Vec<TransferMsg> {
        let mut r = Rechunker::new(size);
        let mut out = r.push(bytes);
        out.extend(r.flush());
        out
    }

    fn manifest(bytes: &[u8]) -> FileManifest {
        FileManifest {
            path: "/f".into(),
            size: bytes.len() as u64,
            sha256: sha256(bytes),
        }
    }

    #[tokio::test]
    async fn download_body_passes_verified_chunks_through() {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i % 7) as u8).collect();
        let body = body_of(&manifest(&bytes), chunks(&bytes, 300)).await;
        assert_eq!(body.len(), 4);
        let joined: Vec<u8> = body.into_iter().flat_map(|b| b.unwrap()).collect();
        assert_eq!(joined, bytes);
    }

    #[tokio::test]
    async fn download_body_withholds_the_last_chunk_when_the_hash_fails() {
        let bytes = vec![7u8; 1000];
        let mut corrupted = bytes.clone();
        corrupted[999] = 0;
        let body = body_of(&manifest(&bytes), chunks(&corrupted, 300)).await;
        let delivered: usize = body
            .iter()
            .filter_map(|b| b.as_ref().ok())
            .map(Bytes::len)
            .sum();
        assert_eq!(delivered, 900, "the client never gets the whole (bad) file");
        assert!(body.last().unwrap().is_err());
    }

    #[tokio::test]
    async fn empty_files_download_as_an_empty_body() {
        assert!(body_of(&manifest(b""), vec![]).await.is_empty());
        let mut wrong = manifest(b"");
        wrong.sha256 = sha256(b"x");
        let body = body_of(&wrong, vec![]).await;
        assert_eq!(body.len(), 1);
        assert!(body[0].is_err());
    }

    #[tokio::test]
    async fn download_body_fails_on_corruption_truncation_and_agent_errors() {
        let bytes: Vec<u8> = (0..1000u32).map(|i| (i % 7) as u8).collect();
        let mut corrupted = bytes.clone();
        corrupted[999] ^= 1;
        let cases = [
            (chunks(&corrupted, 300), "SHA-256"),
            (chunks(&bytes[..700], 300), "closed the stream early"),
            (
                vec![TransferMsg::Error {
                    kind: protocol::transfer::ErrorKind::Integrity,
                    message: "the file shrank".into(),
                }],
                "the file shrank",
            ),
            (
                vec![TransferMsg::Chunk {
                    offset: 5,
                    data: vec![1],
                }],
                "offset 5",
            ),
        ];
        for (frames, why) in cases {
            let body = body_of(&manifest(&bytes), frames).await;
            let last = body.last().expect("some output");
            let err = last.as_ref().expect_err("ends in an error");
            assert!(err.to_string().contains(why), "{err} vs {why}");
            assert_eq!(body.iter().filter(|b| b.is_err()).count(), 1);
        }
    }

    #[test]
    fn sha256_headers_are_parsed_strictly() {
        let hex_digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            parse_sha256(hex_digest),
            Some(protocol::transfer::sha256(b""))
        );
        assert_eq!(
            parse_sha256(&hex_digest.to_uppercase()),
            parse_sha256(hex_digest)
        );
        assert_eq!(parse_sha256(&hex_digest[2..]), None, "too short");
        assert_eq!(parse_sha256("zz"), None);
        assert_eq!(parse_sha256(""), None);
    }
}
