//! Length-delimited framing over any async byte stream (QUIC streams in practice).
//!
//! Frame layout: `[len: u32 big-endian][payload: len bytes of postcard]`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Message;

/// Largest payload accepted in a single frame. Guards against a peer making us
/// allocate an arbitrary amount of memory from a forged length prefix.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds maximum of {MAX_FRAME_LEN}")]
    TooLarge(u64),
    #[error("stream ended in the middle of a frame")]
    Truncated,
    #[error("malformed message: {0}")]
    Decode(#[from] postcard::Error),
}

/// Encode `msg` and write it as one frame. Does not flush or finish the stream.
pub async fn write_frame<W>(writer: &mut W, msg: &Message) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let payload = postcard::to_stdvec(msg)?;
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_LEN)
        .ok_or(FrameError::TooLarge(payload.len() as u64))?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    writer.write_all(&buf).await?;
    Ok(())
}

/// Read one frame and decode it.
///
/// Returns `Ok(None)` if the stream ended cleanly on a frame boundary, and
/// [`FrameError::Truncated`] if it ended part-way through a frame.
///
/// Not cancel-safe: if the future is dropped mid-read, the stream is left at an
/// unknown offset. Run it in its own task rather than inside `select!`.
pub async fn read_frame<R>(reader: &mut R) -> Result<Option<Message>, FrameError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        match reader.read(&mut len_buf[filled..]).await? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(FrameError::Truncated),
            n => filled += n,
        }
    }

    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len.into()));
    }

    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(Some(postcard::from_bytes(&payload)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        vec![
            Message::Hello {
                agent_id: "agent-1".into(),
                version: crate::PROTOCOL_VERSION,
            },
            Message::Heartbeat {
                ts: 1_700_000_000_000,
                seq: 0,
            },
            Message::Heartbeat {
                ts: u64::MAX,
                seq: u64::MAX,
            },
            Message::HeartbeatAck { seq: 42 },
        ]
    }

    #[tokio::test]
    async fn round_trips_every_message_in_sequence() {
        let mut buf = Vec::new();
        for msg in samples() {
            write_frame(&mut buf, &msg).await.unwrap();
        }

        let mut reader = buf.as_slice();
        for expected in samples() {
            let got = read_frame(&mut reader).await.unwrap();
            assert_eq!(got, Some(expected));
        }
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn round_trips_across_a_duplex_pipe_with_tiny_buffer() {
        // An 8-byte pipe forces reads to arrive in many small pieces.
        let (mut client, mut server) = tokio::io::duplex(8);
        let writer = tokio::spawn(async move {
            for msg in samples() {
                write_frame(&mut client, &msg).await.unwrap();
            }
        });
        for expected in samples() {
            assert_eq!(read_frame(&mut server).await.unwrap(), Some(expected));
        }
        writer.await.unwrap();
        assert!(read_frame(&mut server).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_has_big_endian_length_prefix() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Message::HeartbeatAck { seq: 7 })
            .await
            .unwrap();
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(len, buf.len() - 4);
    }

    #[tokio::test]
    async fn empty_stream_is_clean_eof() {
        let mut reader: &[u8] = &[];
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_inside_length_prefix_is_truncated() {
        let mut reader: &[u8] = &[0, 0];
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::Truncated)
        ));
    }

    #[tokio::test]
    async fn eof_inside_payload_is_truncated() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Message::HeartbeatAck { seq: 1 })
            .await
            .unwrap();
        buf.pop();
        let mut reader = buf.as_slice();
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::Truncated)
        ));
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected_without_allocating() {
        let mut reader: &[u8] = &(MAX_FRAME_LEN + 1).to_be_bytes();
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::TooLarge(n)) if n == u64::from(MAX_FRAME_LEN) + 1
        ));
    }

    #[tokio::test]
    async fn garbage_payload_is_a_decode_error() {
        // Variant index 200 does not exist.
        let mut reader: &[u8] = &[0, 0, 0, 1, 200];
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::Decode(_))
        ));
    }
}
