//! Chunked file transfer with a manifest and SHA-256 verification.
//!
//! Each transfer is its own bidirectional stream opened by the server:
//!
//! **Upload** (server → agent), after [`crate::StreamOpen::Upload`]:
//! 1. agent: [`TransferMsg::Ready`] (or `Error`: bad path, file exists, …)
//! 2. server: `Chunk`s in order until `manifest.size` bytes are sent
//! 3. agent: verifies size and hash, moves the file into place, then
//!    [`TransferMsg::Complete`] (or `Error`)
//!
//! **Download** (agent → server), after [`crate::StreamOpen::Download`]:
//! 1. agent: [`TransferMsg::Manifest`] (size and hash, computed by reading
//!    the file once), or `Error`
//! 2. agent: `Chunk`s until `size` bytes are sent; the server verifies
//!
//! Neither side ever holds a whole file: chunks are at most [`CHUNK_SIZE`]
//! and are hashed as they pass. [`ChunkReceiver`] is the receiving side's
//! bookkeeping; [`Rechunker`] cuts arbitrary buffers (HTTP body pieces) into
//! chunks; [`send_chunks`] streams a reader as chunks.

use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};

use crate::{write_frame, FrameError};

/// Payload bytes per `Chunk` frame.
pub const CHUNK_SIZE: usize = 256 * 1024;

pub type Sha256 = [u8; 32];

/// What is (to be) transferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifest {
    /// Absolute path on the agent.
    pub path: String,
    pub size: u64,
    pub sha256: Sha256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRequest {
    pub manifest: FileManifest,
    /// Replace an existing file. Without it an existing file is an error.
    pub overwrite: bool,
}

/// Why the agent refused or failed a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// Relative path, a directory where a file was expected, …
    InvalidPath,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    /// Size or hash did not match the manifest.
    Integrity,
    Io,
}

/// Frames after the `StreamOpen`, in either direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferMsg {
    /// Agent: the upload is accepted; send the chunks.
    Ready,
    /// Agent: the file about to be downloaded.
    Manifest(FileManifest),
    Chunk {
        offset: u64,
        data: Vec<u8>,
    },
    /// Agent: the upload was verified and is in place.
    Complete {
        size: u64,
        sha256: Sha256,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChunkError {
    #[error("chunk at offset {got}, expected {expected}")]
    OutOfOrder { expected: u64, got: u64 },
    #[error("more data than the {size} bytes announced")]
    Overrun { size: u64 },
    #[error("transfer ended after {received} of {size} bytes")]
    Incomplete { received: u64, size: u64 },
    #[error("SHA-256 mismatch: the data does not match the manifest")]
    HashMismatch,
}

/// Receiving side of a transfer: checks that chunks arrive in order, that
/// no more than `size` bytes arrive, and that the result hashes to
/// `sha256`.
pub struct ChunkReceiver {
    size: u64,
    sha256: Sha256,
    received: u64,
    hasher: Context,
}

impl ChunkReceiver {
    pub fn new(size: u64, sha256: Sha256) -> Self {
        Self {
            size,
            sha256,
            received: 0,
            hasher: Context::new(&SHA256),
        }
    }

    pub fn for_manifest(manifest: &FileManifest) -> Self {
        Self::new(manifest.size, manifest.sha256)
    }

    /// Check and hash one chunk. The caller writes `data` only if this succeeds.
    pub fn accept(&mut self, offset: u64, data: &[u8]) -> Result<(), ChunkError> {
        if offset != self.received {
            return Err(ChunkError::OutOfOrder {
                expected: self.received,
                got: offset,
            });
        }
        if data.len() as u64 > self.size - self.received {
            return Err(ChunkError::Overrun { size: self.size });
        }
        self.hasher.update(data);
        self.received += data.len() as u64;
        Ok(())
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    /// All announced bytes have arrived (the hash is not checked yet).
    pub fn is_complete(&self) -> bool {
        self.received == self.size
    }

    /// Verify the whole transfer.
    pub fn finish(self) -> Result<Sha256, ChunkError> {
        if self.received != self.size {
            return Err(ChunkError::Incomplete {
                received: self.received,
                size: self.size,
            });
        }
        let digest = to_sha256(self.hasher.finish());
        if digest != self.sha256 {
            return Err(ChunkError::HashMismatch);
        }
        Ok(digest)
    }
}

/// Cuts a stream of arbitrarily sized buffers into `Chunk` messages of
/// exactly `chunk_size` bytes (the last one may be shorter), numbered by
/// offset.
pub struct Rechunker {
    chunk_size: usize,
    offset: u64,
    pending: Vec<u8>,
}

impl Rechunker {
    pub fn new(chunk_size: usize) -> Self {
        assert!(chunk_size > 0);
        Self {
            chunk_size,
            offset: 0,
            pending: Vec::with_capacity(chunk_size),
        }
    }

    /// Add bytes; returns every chunk that is now full.
    pub fn push(&mut self, mut bytes: &[u8]) -> Vec<TransferMsg> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let take = (self.chunk_size - self.pending.len()).min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.pending.len() == self.chunk_size {
                out.push(self.take_chunk());
            }
        }
        out
    }

    /// The last, partial chunk, if any.
    pub fn flush(&mut self) -> Option<TransferMsg> {
        (!self.pending.is_empty()).then(|| self.take_chunk())
    }

    fn take_chunk(&mut self) -> TransferMsg {
        let data = std::mem::replace(&mut self.pending, Vec::with_capacity(self.chunk_size));
        let offset = self.offset;
        self.offset += data.len() as u64;
        TransferMsg::Chunk { offset, data }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("reading the source: {0}")]
    Read(#[source] std::io::Error),
    #[error("source ended after {sent} of {size} bytes")]
    Short { sent: u64, size: u64 },
    #[error(transparent)]
    Frame(#[from] FrameError),
}

/// Stream exactly `size` bytes of `reader` to `writer` as `Chunk` frames.
/// Returns the SHA-256 of what was sent. Holds one chunk in memory.
pub async fn send_chunks<R, W>(
    reader: &mut R,
    size: u64,
    writer: &mut W,
) -> Result<Sha256, SendError>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut hasher = Context::new(&SHA256);
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut sent = 0u64;
    while sent < size {
        let want = (size - sent).min(CHUNK_SIZE as u64) as usize;
        let n = reader
            .read(&mut buf[..want])
            .await
            .map_err(SendError::Read)?;
        if n == 0 {
            return Err(SendError::Short { sent, size });
        }
        hasher.update(&buf[..n]);
        let msg = TransferMsg::Chunk {
            offset: sent,
            data: buf[..n].to_vec(),
        };
        write_frame(writer, &msg).await?;
        sent += n as u64;
    }
    Ok(to_sha256(hasher.finish()))
}

/// Size and SHA-256 of everything `reader` yields.
pub async fn hash_reader<R>(reader: &mut R) -> std::io::Result<(u64, Sha256)>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut hasher = Context::new(&SHA256);
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok((size, to_sha256(hasher.finish())));
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
}

pub fn sha256(bytes: &[u8]) -> Sha256 {
    to_sha256(ring::digest::digest(&SHA256, bytes))
}

fn to_sha256(digest: ring::digest::Digest) -> Sha256 {
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_frame;

    fn data(len: usize) -> Vec<u8> {
        // Not periodic in the chunk size, so misordered chunks change the hash.
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn receiver_accepts_in_order_chunks_and_verifies_the_hash() {
        let bytes = data(1000);
        let mut rx = ChunkReceiver::new(1000, sha256(&bytes));
        for (i, chunk) in bytes.chunks(300).enumerate() {
            assert!(!rx.is_complete());
            rx.accept(i as u64 * 300, chunk).unwrap();
        }
        assert!(rx.is_complete());
        assert_eq!(rx.received(), 1000);
        assert_eq!(rx.finish(), Ok(sha256(&bytes)));
    }

    #[test]
    fn receiver_rejects_gaps_repeats_and_overruns() {
        let bytes = data(10);
        let mut rx = ChunkReceiver::new(10, sha256(&bytes));
        assert_eq!(
            rx.accept(4, &bytes[4..]),
            Err(ChunkError::OutOfOrder {
                expected: 0,
                got: 4
            })
        );
        rx.accept(0, &bytes[..4]).unwrap();
        assert_eq!(
            rx.accept(0, &bytes[..4]),
            Err(ChunkError::OutOfOrder {
                expected: 4,
                got: 0
            }),
            "a repeated chunk"
        );
        assert_eq!(
            rx.accept(4, &data(7)),
            Err(ChunkError::Overrun { size: 10 })
        );
        // Rejected chunks are not counted.
        assert_eq!(rx.received(), 4);
        rx.accept(4, &bytes[4..]).unwrap();
        assert!(rx.finish().is_ok());
    }

    #[test]
    fn receiver_detects_short_and_corrupted_transfers() {
        let bytes = data(10);
        let mut short = ChunkReceiver::new(10, sha256(&bytes));
        short.accept(0, &bytes[..9]).unwrap();
        assert_eq!(
            short.finish(),
            Err(ChunkError::Incomplete {
                received: 9,
                size: 10
            })
        );

        let mut corrupt = bytes.clone();
        corrupt[5] ^= 1;
        let mut rx = ChunkReceiver::new(10, sha256(&bytes));
        rx.accept(0, &corrupt).unwrap();
        assert_eq!(rx.finish(), Err(ChunkError::HashMismatch));
    }

    #[test]
    fn empty_files_transfer_with_no_chunks() {
        let rx = ChunkReceiver::new(0, sha256(b""));
        assert!(rx.is_complete());
        assert_eq!(rx.finish(), Ok(sha256(b"")));
        assert_eq!(Rechunker::new(4).flush(), None);
    }

    #[test]
    fn rechunker_emits_fixed_size_chunks_whatever_the_input_pieces() {
        let bytes = data(23);
        // Pieces smaller than, equal to and larger than a chunk.
        let mut r = Rechunker::new(5);
        let mut chunks = Vec::new();
        for piece in [
            &bytes[..2],
            &bytes[2..7],
            &bytes[7..8],
            &bytes[8..21],
            &bytes[21..],
        ] {
            chunks.extend(r.push(piece));
        }
        chunks.extend(r.flush());
        let mut expected_offset = 0;
        let mut joined = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            let TransferMsg::Chunk { offset, data } = c else {
                panic!("not a chunk")
            };
            assert_eq!(*offset, expected_offset);
            let last = i == chunks.len() - 1;
            assert!(if last {
                data.len() <= 5
            } else {
                data.len() == 5
            });
            expected_offset += data.len() as u64;
            joined.extend_from_slice(data);
        }
        assert_eq!(chunks.len(), 5);
        assert_eq!(joined, bytes);
        assert_eq!(r.flush(), None, "flush leaves nothing behind");
    }

    #[tokio::test]
    async fn send_chunks_streams_a_large_reader_that_the_receiver_verifies() {
        // Several chunks plus a partial one, through a small pipe so reads
        // and writes interleave.
        let bytes = data(CHUNK_SIZE * 3 + 12_345);
        let size = bytes.len() as u64;
        let expected = sha256(&bytes);
        let (mut tx, mut rx) = tokio::io::duplex(64 * 1024);
        let source = bytes.clone();
        let sender = tokio::spawn(async move {
            let digest = send_chunks(&mut source.as_slice(), size, &mut tx)
                .await
                .unwrap();
            drop(tx);
            digest
        });

        let mut receiver = ChunkReceiver::new(size, expected);
        let mut frames = 0;
        while let Some(msg) = read_frame::<_, TransferMsg>(&mut rx).await.unwrap() {
            let TransferMsg::Chunk { offset, data } = msg else {
                panic!("not a chunk")
            };
            assert!(data.len() <= CHUNK_SIZE);
            receiver.accept(offset, &data).unwrap();
            frames += 1;
        }
        assert_eq!(sender.await.unwrap(), expected);
        assert_eq!(frames, 4);
        assert_eq!(receiver.finish(), Ok(expected));
    }

    #[tokio::test]
    async fn send_chunks_reports_a_source_that_is_shorter_than_announced() {
        let bytes = data(100);
        let mut sink = Vec::new();
        let err = send_chunks(&mut bytes.as_slice(), 150, &mut sink)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SendError::Short {
                    sent: 100,
                    size: 150
                }
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn send_chunks_sends_only_the_announced_size() {
        // A file that grew after it was hashed: the extra bytes are not sent.
        let bytes = data(100);
        let mut sink = Vec::new();
        let digest = send_chunks(&mut bytes.as_slice(), 60, &mut sink)
            .await
            .unwrap();
        assert_eq!(digest, sha256(&bytes[..60]));
    }

    #[tokio::test]
    async fn hash_reader_matches_a_one_shot_hash() {
        let bytes = data(CHUNK_SIZE + 1);
        assert_eq!(
            hash_reader(&mut bytes.as_slice()).await.unwrap(),
            (bytes.len() as u64, sha256(&bytes))
        );
        assert_eq!(
            hash_reader(&mut (&[] as &[u8])).await.unwrap(),
            (0, sha256(b""))
        );
    }
}
