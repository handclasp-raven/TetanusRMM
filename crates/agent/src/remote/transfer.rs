//! The agent's end of file transfers (see `protocol::transfer`).
//!
//! Uploads are written to a temporary file next to the target and moved into
//! place only once the size and SHA-256 match the manifest, so a failed or
//! interrupted upload never leaves a partial file at the target path.
//! Downloads are read twice: once to hash for the manifest, once to send.
//! Neither holds more than a chunk in memory.
//!
//! Paths must be absolute. Under the Windows service, files are read and
//! written as SYSTEM.

use std::path::{Path, PathBuf};

use protocol::transfer::{
    hash_reader, send_chunks, ChunkError, ChunkReceiver, ErrorKind, FileManifest, SendError,
    TransferMsg, UploadRequest,
};
use protocol::{read_frame, write_frame, FrameError};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tracing::{info, warn};

/// A refusal or failure to report to the server.
#[derive(Debug)]
struct Failure {
    kind: ErrorKind,
    message: String,
}

impl Failure {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn io(context: &str, e: std::io::Error) -> Self {
        let kind = match e.kind() {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
            _ => ErrorKind::Io,
        };
        Self::new(kind, format!("{context}: {e}"))
    }

    fn into_msg(self) -> TransferMsg {
        TransferMsg::Error {
            kind: self.kind,
            message: self.message,
        }
    }
}

impl From<ChunkError> for Failure {
    fn from(e: ChunkError) -> Self {
        Self::new(ErrorKind::Integrity, e.to_string())
    }
}

/// Either the transfer failed (tell the server) or the stream did (nobody
/// to tell).
enum Error {
    Failure(Failure),
    Stream(FrameError),
}

impl From<Failure> for Error {
    fn from(f: Failure) -> Self {
        Error::Failure(f)
    }
}

impl From<ChunkError> for Error {
    fn from(e: ChunkError) -> Self {
        Error::Failure(e.into())
    }
}

impl From<FrameError> for Error {
    fn from(e: FrameError) -> Self {
        Error::Stream(e)
    }
}

fn absolute(path: &str) -> Result<PathBuf, Failure> {
    let path = PathBuf::from(path);
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(Failure::new(
            ErrorKind::InvalidPath,
            "path must be an absolute path to a file",
        ));
    }
    Ok(path)
}

/// Send `result`'s error to the server, if it is one the server should hear.
async fn report<W>(send: &mut W, result: Result<(), Error>) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
{
    match result {
        Ok(()) => Ok(()),
        Err(Error::Failure(f)) => {
            warn!(kind = ?f.kind, "transfer failed: {}", f.message);
            write_frame(send, &f.into_msg()).await
        }
        Err(Error::Stream(e)) => Err(e),
    }
}

// --- Upload ------------------------------------------------------------------

/// Removes the temporary file unless the upload was committed.
struct Partial {
    path: PathBuf,
    committed: bool,
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Receive a file from the server.
pub async fn upload<W, R>(
    request: UploadRequest,
    send: &mut W,
    recv: &mut R,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    let result = receive(&request, send, recv).await;
    report(send, result).await
}

async fn receive<W, R>(request: &UploadRequest, send: &mut W, recv: &mut R) -> Result<(), Error>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    let manifest = &request.manifest;
    let target = absolute(&manifest.path)?;
    check_target(&target, request.overwrite).await?;

    let partial_path = partial_name(&target);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial_path)
        .await
        .map_err(|e| Failure::io("creating the file", e))?;
    let mut partial = Partial {
        path: partial_path,
        committed: false,
    };
    info!(path = %target.display(), size = manifest.size, "upload started");
    write_frame(send, &TransferMsg::Ready).await?;

    let mut receiver = ChunkReceiver::for_manifest(manifest);
    while !receiver.is_complete() {
        match read_frame::<_, TransferMsg>(recv).await? {
            Some(TransferMsg::Chunk { offset, data }) => {
                receiver.accept(offset, &data)?;
                file.write_all(&data)
                    .await
                    .map_err(|e| Failure::io("writing the file", e))?;
            }
            Some(other) => {
                return Err(Failure::new(
                    ErrorKind::Integrity,
                    format!("expected a chunk, got {}", describe(&other)),
                )
                .into())
            }
            // The server gave up (or went away) part-way.
            None => break,
        }
    }
    file.sync_all()
        .await
        .map_err(|e| Failure::io("flushing the file", e))?;
    drop(file);
    let size = receiver.received();
    let sha256 = receiver.finish()?;

    // Re-check: something may have created the target meanwhile.
    check_target(&target, request.overwrite).await?;
    tokio::fs::rename(&partial.path, &target)
        .await
        .map_err(|e| Failure::io("moving the file into place", e))?;
    partial.committed = true;
    info!(path = %target.display(), size, "upload verified and saved");
    write_frame(send, &TransferMsg::Complete { size, sha256 }).await?;
    Ok(())
}

async fn check_target(target: &Path, overwrite: bool) -> Result<(), Failure> {
    match tokio::fs::metadata(target).await {
        Ok(meta) if meta.is_dir() => Err(Failure::new(
            ErrorKind::InvalidPath,
            "the path is a directory",
        )),
        Ok(_) if !overwrite => Err(Failure::new(
            ErrorKind::AlreadyExists,
            "the file already exists (set overwrite to replace it)",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = target.parent().unwrap_or(target);
            match tokio::fs::metadata(parent).await {
                Ok(meta) if meta.is_dir() => Ok(()),
                _ => Err(Failure::new(
                    ErrorKind::NotFound,
                    "the parent directory does not exist",
                )),
            }
        }
        Err(e) => Err(Failure::io("checking the path", e)),
    }
}

/// A unique temporary name in the target's directory, so the final rename
/// stays on one volume.
fn partial_name(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".rmm-upload-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    target.with_file_name(name)
}

fn describe(msg: &TransferMsg) -> &'static str {
    match msg {
        TransferMsg::Ready => "Ready",
        TransferMsg::Manifest(_) => "Manifest",
        TransferMsg::Chunk { .. } => "Chunk",
        TransferMsg::Complete { .. } => "Complete",
        TransferMsg::Error { .. } => "Error",
    }
}

// --- Download ----------------------------------------------------------------

/// Send a file to the server.
pub async fn download<W>(path: &str, send: &mut W) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
{
    let result = transmit(path, send).await;
    report(send, result).await
}

async fn transmit<W>(path: &str, send: &mut W) -> Result<(), Error>
where
    W: AsyncWrite + Unpin + Send,
{
    let source = absolute(path)?;
    let open = || async {
        // Check first: opening a directory fails with "access denied" on Windows.
        let meta = tokio::fs::metadata(&source)
            .await
            .map_err(|e| Failure::io("opening the file", e))?;
        if !meta.is_file() {
            return Err(Failure::new(ErrorKind::InvalidPath, "not a regular file"));
        }
        tokio::fs::File::open(&source)
            .await
            .map_err(|e| Failure::io("opening the file", e))
    };

    let (size, sha256) = hash_reader(&mut open().await?)
        .await
        .map_err(|e| Failure::io("reading the file", e))?;
    let manifest = FileManifest {
        path: path.to_owned(),
        size,
        sha256,
    };
    info!(path = %source.display(), size, "download started");
    write_frame(send, &TransferMsg::Manifest(manifest)).await?;

    let mut file = open().await?;
    match send_chunks(&mut file, size, send).await {
        Ok(sent) if sent == sha256 => {
            info!(path = %source.display(), size, "download sent");
            Ok(())
        }
        // The server's own check fails too, but say why.
        Ok(_) => Err(Failure::new(
            ErrorKind::Integrity,
            "the file changed while it was being sent",
        )
        .into()),
        Err(SendError::Short { .. }) => Err(Failure::new(
            ErrorKind::Integrity,
            "the file shrank while it was being sent",
        )
        .into()),
        Err(SendError::Read(e)) => Err(Failure::io("reading the file", e).into()),
        Err(SendError::Frame(e)) => Err(Error::Stream(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::transfer::{sha256, Rechunker};
    use tokio::io::DuplexStream;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
    }

    fn manifest(path: &Path, bytes: &[u8]) -> FileManifest {
        FileManifest {
            path: path.to_str().unwrap().into(),
            size: bytes.len() as u64,
            sha256: sha256(bytes),
        }
    }

    struct Streams {
        agent_send: DuplexStream,
        agent_recv: DuplexStream,
        server_send: DuplexStream,
        server_recv: DuplexStream,
    }

    fn streams() -> Streams {
        let (agent_send, server_recv) = tokio::io::duplex(1 << 20);
        let (server_send, agent_recv) = tokio::io::duplex(1 << 20);
        Streams {
            agent_send,
            agent_recv,
            server_send,
            server_recv,
        }
    }

    async fn recv(r: &mut DuplexStream) -> TransferMsg {
        read_frame(r).await.unwrap().expect("a message")
    }

    /// Upload `sent` (announcing `announced`) and return the agent's replies.
    async fn upload_bytes(request: UploadRequest, sent: &[u8]) -> Vec<TransferMsg> {
        let Streams {
            mut agent_send,
            mut agent_recv,
            mut server_send,
            mut server_recv,
        } = streams();
        let agent =
            tokio::spawn(async move { upload(request, &mut agent_send, &mut agent_recv).await });
        let mut replies = vec![recv(&mut server_recv).await];
        if replies[0] == TransferMsg::Ready {
            let mut chunker = Rechunker::new(1000);
            for chunk in chunker.push(sent).into_iter().chain(chunker.flush()) {
                write_frame(&mut server_send, &chunk).await.unwrap();
            }
            drop(server_send);
            replies.push(recv(&mut server_recv).await);
        }
        agent.await.unwrap().unwrap();
        replies
    }

    #[tokio::test]
    async fn upload_is_verified_before_it_appears_at_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.bin");
        let bytes = data(10_500);
        let replies = upload_bytes(
            UploadRequest {
                manifest: manifest(&target, &bytes),
                overwrite: false,
            },
            &bytes,
        )
        .await;
        assert_eq!(
            replies,
            [
                TransferMsg::Ready,
                TransferMsg::Complete {
                    size: bytes.len() as u64,
                    sha256: sha256(&bytes)
                }
            ]
        );
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[tokio::test]
    async fn corrupted_or_short_uploads_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.bin");
        let bytes = data(5000);
        let mut corrupted = bytes.clone();
        corrupted[4321] ^= 0xFF;

        for (sent, why) in [
            (&corrupted[..], "SHA-256"),
            (&bytes[..4000], "ended after 4000"),
        ] {
            let replies = upload_bytes(
                UploadRequest {
                    manifest: manifest(&target, &bytes),
                    overwrite: false,
                },
                sent,
            )
            .await;
            match &replies[1] {
                TransferMsg::Error {
                    kind: ErrorKind::Integrity,
                    message,
                } => assert!(message.contains(why), "{message}"),
                other => panic!("expected an integrity error, got {other:?}"),
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn upload_refuses_bad_targets_before_any_data() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("exists.txt");
        std::fs::write(&existing, b"keep me").unwrap();
        let bytes = b"new".to_vec();
        let cases = [
            (existing.clone(), false, ErrorKind::AlreadyExists),
            (dir.path().to_path_buf(), true, ErrorKind::InvalidPath),
            (dir.path().join("missing/dir/f"), false, ErrorKind::NotFound),
            (PathBuf::from("relative.txt"), false, ErrorKind::InvalidPath),
        ];
        for (path, overwrite, expected) in cases {
            let replies = upload_bytes(
                UploadRequest {
                    manifest: manifest(&path, &bytes),
                    overwrite,
                },
                &bytes,
            )
            .await;
            assert!(
                matches!(replies[..], [TransferMsg::Error { kind, .. }] if kind == expected),
                "{path:?}: {replies:?}"
            );
        }
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");

        // With overwrite it is replaced.
        let replies = upload_bytes(
            UploadRequest {
                manifest: manifest(&existing, &bytes),
                overwrite: true,
            },
            &bytes,
        )
        .await;
        assert!(matches!(replies[1], TransferMsg::Complete { .. }));
        assert_eq!(std::fs::read(&existing).unwrap(), b"new");
    }

    #[tokio::test]
    async fn download_sends_a_manifest_then_verifiable_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("log.txt");
        let bytes = data(protocol::transfer::CHUNK_SIZE * 2 + 17);
        std::fs::write(&source, &bytes).unwrap();

        let Streams {
            mut agent_send,
            mut server_recv,
            ..
        } = streams();
        let path = source.to_str().unwrap().to_owned();
        let agent = tokio::spawn(async move { download(&path, &mut agent_send).await });

        let TransferMsg::Manifest(m) = recv(&mut server_recv).await else {
            panic!("expected a manifest")
        };
        assert_eq!(m, manifest(&source, &bytes));
        let mut receiver = ChunkReceiver::for_manifest(&m);
        let mut received = Vec::new();
        while !receiver.is_complete() {
            let TransferMsg::Chunk { offset, data } = recv(&mut server_recv).await else {
                panic!("expected a chunk")
            };
            receiver.accept(offset, &data).unwrap();
            received.extend(data);
        }
        assert_eq!(receiver.finish(), Ok(sha256(&bytes)));
        assert_eq!(received, bytes);
        agent.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn download_errors_are_reported_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        for (path, expected) in [
            (dir.path().join("nope"), ErrorKind::NotFound),
            (dir.path().to_path_buf(), ErrorKind::InvalidPath),
            (PathBuf::from("rel/path"), ErrorKind::InvalidPath),
        ] {
            let Streams {
                mut agent_send,
                mut server_recv,
                ..
            } = streams();
            download(path.to_str().unwrap(), &mut agent_send)
                .await
                .unwrap();
            assert!(
                matches!(recv(&mut server_recv).await, TransferMsg::Error { kind, .. } if kind == expected),
                "{path:?}"
            );
        }
    }
}
