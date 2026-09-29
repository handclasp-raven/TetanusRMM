//! Stream multiplexing over one WebSocket: the subset of QUIC the RMM
//! protocol uses, for networks where UDP is blocked.
//!
//! Every WebSocket binary message is one mux frame:
//!
//! ```text
//! [kind: u8][stream id: u32 BE][payload]
//! ```
//!
//! | kind | payload | meaning |
//! |---|---|---|
//! | `OPEN_BI` / `OPEN_UNI` | - | the sender opened a stream |
//! | `DATA` | bytes | stream data, at most [`MAX_CHUNK`] |
//! | `FIN` | - | the sender finished writing (end of stream) |
//! | `RESET` | code u32 | the sender abandoned writing |
//! | `STOP` | code u32 | the receiver stopped reading; writes now fail |
//! | `WINDOW` | increment u32 | the receiver consumed data: more credit |
//! | `CLOSE` | code u32, reason | connection closed (stream id 0) |
//!
//! Client-opened streams have even ids and server-opened streams odd ones,
//! so the two ends never collide.
//!
//! **Flow control.** Each stream direction starts with [`STREAM_WINDOW`]
//! bytes of credit. A writer that runs out waits until the reader consumes
//! data and returns credit with `WINDOW`. So a stalled reader (a slow
//! viewer, an agent writing a file) bounds how much of its stream is
//! buffered, and never holds up the other streams on the connection.
//!
//! **Priority.** Frames for stream 0 (the first stream the client opens,
//! which is always the control stream: heartbeats, input, consent) and
//! stream-independent frames (`WINDOW`, `STOP`, `CLOSE`) jump ahead of bulk
//! data such as video or file transfers. A stream's `OPEN`/`DATA`/`FIN`/
//! `RESET` always share one queue, so they stay in order.
//!
//! **Liveness.** Each end pings every [`KEEP_ALIVE`]; a connection that
//! receives nothing for [`IDLE_TIMEOUT`] is dead, like a QUIC idle timeout.
//!
//! Dropping every handle to a connection (the [`Connection`] clones and all
//! its streams) closes it with code 0, as quinn does. Dropping a
//! [`SendStream`] without finishing it finishes it; dropping a
//! [`RecvStream`] before the end tells the writer to stop.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, watch, Notify};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use crate::ConnectionError;

/// Credit each stream direction starts with, and the most a reader buffers.
pub const STREAM_WINDOW: u32 = 256 * 1024;

/// Return credit once this much has been consumed, not on every read.
const CREDIT_BATCH: u32 = STREAM_WINDOW / 4;

/// Largest `DATA` payload. Small enough that control-stream frames never
/// wait long behind a bulk frame already being written.
pub const MAX_CHUNK: usize = 16 * 1024;

/// How often each end pings.
pub const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// A connection that receives nothing for this long is dead.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Close code for a mux-level protocol violation; the peer reports it as a
/// transport error, not an application close.
pub const MUX_ERROR: u32 = 0xFFFF_FF01;

/// Frames written per flush: bounds how long a flush is deferred.
const WRITE_BATCH: usize = 32;

const OPEN_BI: u8 = 1;
const OPEN_UNI: u8 = 2;
const DATA: u8 = 3;
const FIN: u8 = 4;
const RESET: u8 = 5;
const STOP: u8 = 6;
const WINDOW: u8 = 7;
const CLOSE: u8 = 8;

const HEADER: usize = 5;

/// Which end of the WebSocket this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Server,
}

impl Side {
    /// Parity of the stream ids this side opens.
    fn parity(self) -> u32 {
        match self {
            Side::Client => 0,
            Side::Server => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    OpenBi(u32),
    OpenUni(u32),
    Data(u32, Bytes),
    Fin(u32),
    Reset(u32, u32),
    Stop(u32, u32),
    Window(u32, u32),
    Close(u32, String),
}

fn encode(kind: u8, id: u32, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(HEADER + payload.len());
    buf.put_u8(kind);
    buf.put_u32(id);
    buf.put_slice(payload);
    buf.freeze()
}

fn encode_code(kind: u8, id: u32, code: u32) -> Bytes {
    encode(kind, id, &code.to_be_bytes())
}

fn decode(mut bytes: Bytes) -> Option<Frame> {
    if bytes.len() < HEADER {
        return None;
    }
    let kind = bytes[0];
    let id = u32::from_be_bytes(bytes[1..HEADER].try_into().ok()?);
    let payload = bytes.split_off(HEADER);
    let code = || Some(u32::from_be_bytes(payload.get(..4)?.try_into().ok()?));
    Some(match kind {
        OPEN_BI if payload.is_empty() => Frame::OpenBi(id),
        OPEN_UNI if payload.is_empty() => Frame::OpenUni(id),
        DATA => Frame::Data(id, payload),
        FIN if payload.is_empty() => Frame::Fin(id),
        RESET if payload.len() == 4 => Frame::Reset(id, code()?),
        STOP if payload.len() == 4 => Frame::Stop(id, code()?),
        WINDOW if payload.len() == 4 => Frame::Window(id, code()?),
        CLOSE if payload.len() >= 4 => {
            Frame::Close(code()?, String::from_utf8_lossy(&payload[4..]).into_owned())
        }
        _ => return None,
    })
}

/// The receiving half of a stream.
#[derive(Debug, Default)]
struct RecvHalf {
    buf: VecDeque<Bytes>,
    buffered: usize,
    fin: bool,
    reset: Option<u32>,
    /// Consumed since credit was last returned.
    consumed: u32,
    waker: Option<Waker>,
}

/// The sending half of a stream.
#[derive(Debug)]
struct SendHalf {
    credit: u64,
    /// Finished or reset: no more writes.
    done: bool,
    /// The reader stopped, with this code.
    stopped: Option<u32>,
    waker: Option<Waker>,
}

impl SendHalf {
    fn new() -> Self {
        Self {
            credit: STREAM_WINDOW.into(),
            done: false,
            stopped: None,
            waker: None,
        }
    }
}

#[derive(Debug, Default)]
struct StreamState {
    recv: Option<RecvHalf>,
    send: Option<SendHalf>,
}

#[derive(Debug, Default)]
struct State {
    /// Next id for a stream this side opens.
    next_id: u32,
    /// Highest id the peer has opened, to reject reuse.
    peer_high: Option<u32>,
    streams: HashMap<u32, StreamState>,
    incoming_bi: VecDeque<u32>,
    incoming_uni: VecDeque<u32>,
    error: Option<ConnectionError>,
}

impl State {
    fn wake_all(&mut self) {
        for stream in self.streams.values_mut() {
            if let Some(w) = stream.recv.as_mut().and_then(|r| r.waker.take()) {
                w.wake();
            }
            if let Some(w) = stream.send.as_mut().and_then(|s| s.waker.take()) {
                w.wake();
            }
        }
    }

    /// Forget a stream once neither half has a handle.
    fn reap(&mut self, id: u32) {
        if self
            .streams
            .get(&id)
            .is_some_and(|s| s.recv.is_none() && s.send.is_none())
        {
            self.streams.remove(&id);
        }
    }
}

struct Shared {
    side: Side,
    remote: SocketAddr,
    stable_id: usize,
    state: Mutex<State>,
    /// Control stream and stream-independent frames.
    urgent: mpsc::UnboundedSender<Bytes>,
    /// Everything else.
    bulk: mpsc::UnboundedSender<Bytes>,
    accept_bi: Notify,
    accept_uni: Notify,
    closed: watch::Sender<Option<ConnectionError>>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue for a stream's own frames.
    fn queue(&self, id: u32) -> &mpsc::UnboundedSender<Bytes> {
        if id == 0 {
            &self.urgent
        } else {
            &self.bulk
        }
    }

    /// Record why the connection ended (the first reason wins) and wake
    /// everything waiting on it.
    fn fail(&self, error: ConnectionError) {
        let mut state = self.state();
        if state.error.is_some() {
            return;
        }
        debug!(stable_id = self.stable_id, %error, "websocket connection closed");
        state.error = Some(error.clone());
        state.wake_all();
        drop(state);
        self.accept_bi.notify_waiters();
        self.accept_uni.notify_waiters();
        self.closed.send_replace(Some(error));
    }

    fn close_local(&self, code: u32, reason: &[u8]) {
        if self.state().error.is_some() {
            return;
        }
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(reason);
        let _ = self.urgent.send(encode(CLOSE, 0, &payload));
        self.fail(ConnectionError::LocallyClosed);
    }

    /// Apply one frame from the peer. An error is a protocol violation.
    fn on_frame(&self, frame: Frame) -> Result<(), &'static str> {
        let mut state = self.state();
        match frame {
            Frame::OpenBi(id) | Frame::OpenUni(id) => {
                let bi = matches!(frame, Frame::OpenBi(_));
                if id % 2 == self.side.parity() || state.peer_high.is_some_and(|h| id <= h) {
                    return Err("invalid stream id");
                }
                state.peer_high = Some(id);
                state.streams.insert(
                    id,
                    StreamState {
                        recv: Some(RecvHalf::default()),
                        send: bi.then(SendHalf::new),
                    },
                );
                if bi {
                    state.incoming_bi.push_back(id);
                    drop(state);
                    self.accept_bi.notify_one();
                } else {
                    state.incoming_uni.push_back(id);
                    drop(state);
                    self.accept_uni.notify_one();
                }
            }
            Frame::Data(id, bytes) => {
                // No receiver: we stopped the stream and the data crossed
                // our STOP in flight. Drop it.
                let Some(recv) = state.streams.get_mut(&id).and_then(|s| s.recv.as_mut()) else {
                    return Ok(());
                };
                if recv.fin || recv.reset.is_some() {
                    return Err("data after end of stream");
                }
                recv.buffered += bytes.len();
                if recv.buffered > STREAM_WINDOW as usize {
                    return Err("flow control window exceeded");
                }
                if !bytes.is_empty() {
                    recv.buf.push_back(bytes);
                }
                if let Some(w) = recv.waker.take() {
                    w.wake();
                }
            }
            Frame::Fin(id) | Frame::Reset(id, _) => {
                if let Some(recv) = state.streams.get_mut(&id).and_then(|s| s.recv.as_mut()) {
                    if let Frame::Reset(_, code) = frame {
                        recv.reset = Some(code);
                        recv.buf.clear();
                        recv.buffered = 0;
                    } else {
                        recv.fin = true;
                    }
                    if let Some(w) = recv.waker.take() {
                        w.wake();
                    }
                }
            }
            Frame::Stop(id, code) => {
                if let Some(send) = state.streams.get_mut(&id).and_then(|s| s.send.as_mut()) {
                    send.stopped = Some(code);
                    if let Some(w) = send.waker.take() {
                        w.wake();
                    }
                }
            }
            Frame::Window(id, increment) => {
                if let Some(send) = state.streams.get_mut(&id).and_then(|s| s.send.as_mut()) {
                    send.credit += u64::from(increment);
                    if send.credit > u64::from(STREAM_WINDOW) {
                        return Err("credit beyond the stream window");
                    }
                    if let Some(w) = send.waker.take() {
                        w.wake();
                    }
                }
            }
            Frame::Close(code, reason) => {
                drop(state);
                self.fail(if code == MUX_ERROR {
                    ConnectionError::Transport(format!("peer reported: {reason}"))
                } else {
                    ConnectionError::ApplicationClosed { code, reason }
                });
            }
        }
        Ok(())
    }
}

/// Keeps a connection open while any handle to it exists.
struct Guard {
    shared: Arc<Shared>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.shared.close_local(0, b"");
    }
}

/// A multiplexed connection over a WebSocket. Cheap to clone.
#[derive(Clone)]
pub struct Connection {
    guard: Arc<Guard>,
}

static NEXT_STABLE_ID: AtomicUsize = AtomicUsize::new(1);

/// Start multiplexing over `ws`: spawns the task that drives the socket.
pub fn spawn<S>(ws: WebSocketStream<S>, side: Side, remote: SocketAddr) -> Connection
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (urgent, urgent_rx) = mpsc::unbounded_channel();
    let (bulk, bulk_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared {
        side,
        remote,
        // High bit set: never equal to a quinn stable id in the same log.
        stable_id: NEXT_STABLE_ID.fetch_add(1, Ordering::Relaxed) | (1 << (usize::BITS - 1)),
        state: Mutex::new(State {
            next_id: side.parity(),
            ..State::default()
        }),
        urgent,
        bulk,
        accept_bi: Notify::new(),
        accept_uni: Notify::new(),
        closed: watch::channel(None).0,
    });
    tokio::spawn(drive(ws, shared.clone(), urgent_rx, bulk_rx));
    Connection {
        guard: Arc::new(Guard { shared }),
    }
}

/// Pump frames between the WebSocket and the streams until the connection
/// ends.
async fn drive<S>(
    ws: WebSocketStream<S>,
    shared: Arc<Shared>,
    mut urgent: mpsc::UnboundedReceiver<Bytes>,
    mut bulk: mpsc::UnboundedReceiver<Bytes>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();

    let writer = async {
        let mut keep_alive = tokio::time::interval(KEEP_ALIVE);
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let first = tokio::select! {
                biased;
                Some(frame) = urgent.recv() => frame,
                Some(frame) = bulk.recv() => frame,
                _ = keep_alive.tick() => {
                    sink.send(WsMessage::Ping(Bytes::new())).await?;
                    continue;
                }
            };
            let mut next = Some(first);
            let mut written = 0;
            while let Some(frame) = next.take() {
                let is_close = frame.first() == Some(&CLOSE);
                sink.feed(WsMessage::Binary(frame)).await?;
                if is_close {
                    // Nothing after a close; say goodbye properly.
                    let _ = sink.send(WsMessage::Close(None)).await;
                    return Ok(());
                }
                written += 1;
                if written < WRITE_BATCH {
                    next = urgent.try_recv().or_else(|_| bulk.try_recv()).ok();
                }
            }
            sink.flush().await?;
        }
    };

    let reader = async {
        loop {
            let message = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Err(_) => return Err(ConnectionError::TimedOut),
                Ok(None) => return Err(ConnectionError::Transport("connection lost".into())),
                Ok(Some(Err(e))) => return Err(ConnectionError::Transport(e.to_string())),
                Ok(Some(Ok(message))) => message,
            };
            match message {
                WsMessage::Binary(bytes) => {
                    let result = match decode(bytes) {
                        Some(frame) => shared.on_frame(frame),
                        None => Err("malformed frame"),
                    };
                    if let Err(violation) = result {
                        return Err(ConnectionError::Transport(format!(
                            "protocol violation: {violation}"
                        )));
                    }
                    if shared.state().error.is_some() {
                        // The peer closed the connection.
                        return Ok(());
                    }
                }
                // tungstenite answers pings itself; any message proves life.
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
                WsMessage::Close(_) => {
                    return Err(ConnectionError::Transport("connection lost".into()))
                }
                WsMessage::Text(_) => {
                    return Err(ConnectionError::Transport(
                        "protocol violation: text message".into(),
                    ))
                }
            }
        }
    };

    let result: Result<(), ConnectionError> = tokio::select! {
        r = writer => r.map_err(|e: tokio_tungstenite::tungstenite::Error| {
            ConnectionError::Transport(e.to_string())
        }),
        r = reader => r,
    };
    if let Err(error) = result {
        if let ConnectionError::Transport(reason) = &error {
            if reason.starts_with("protocol violation") {
                // Best effort: tell the peer why.
                let mut payload = MUX_ERROR.to_be_bytes().to_vec();
                payload.extend_from_slice(reason.as_bytes());
                let _ = sink
                    .send(WsMessage::Binary(encode(CLOSE, 0, &payload)))
                    .await;
            }
        }
        shared.fail(error);
    }
    let _ = sink.close().await;
}

impl Connection {
    fn shared(&self) -> &Arc<Shared> {
        &self.guard.shared
    }

    fn open(&self, bi: bool) -> Result<u32, ConnectionError> {
        let shared = self.shared();
        let mut state = shared.state();
        if let Some(e) = &state.error {
            return Err(e.clone());
        }
        let id = state.next_id;
        state.next_id = id
            .checked_add(2)
            .ok_or_else(|| ConnectionError::Transport("stream ids exhausted".into()))?;
        state.streams.insert(
            id,
            StreamState {
                recv: bi.then(RecvHalf::default),
                send: Some(SendHalf::new()),
            },
        );
        let _ = shared
            .queue(id)
            .send(encode(if bi { OPEN_BI } else { OPEN_UNI }, id, &[]));
        Ok(id)
    }

    fn send_stream(&self, id: u32) -> SendStream {
        SendStream {
            guard: self.guard.clone(),
            id,
        }
    }

    fn recv_stream(&self, id: u32) -> RecvStream {
        RecvStream {
            guard: self.guard.clone(),
            id,
        }
    }

    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        let id = self.open(true)?;
        Ok((self.send_stream(id), self.recv_stream(id)))
    }

    pub async fn open_uni(&self) -> Result<SendStream, ConnectionError> {
        let id = self.open(false)?;
        Ok(self.send_stream(id))
    }

    async fn accept(&self, bi: bool) -> Result<u32, ConnectionError> {
        let shared = self.shared();
        let notify = if bi {
            &shared.accept_bi
        } else {
            &shared.accept_uni
        };
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            // Register before checking, so a close in between still wakes us.
            notified.as_mut().enable();
            {
                let mut state = shared.state();
                let queue = if bi {
                    &mut state.incoming_bi
                } else {
                    &mut state.incoming_uni
                };
                if let Some(id) = queue.pop_front() {
                    return Ok(id);
                }
                if let Some(e) = &state.error {
                    return Err(e.clone());
                }
            }
            notified.await;
        }
    }

    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream), ConnectionError> {
        let id = self.accept(true).await?;
        Ok((self.send_stream(id), self.recv_stream(id)))
    }

    pub async fn accept_uni(&self) -> Result<RecvStream, ConnectionError> {
        let id = self.accept(false).await?;
        Ok(self.recv_stream(id))
    }

    /// Close the connection, telling the peer `code` and `reason`. Queued
    /// data that has not been written yet may be discarded.
    pub fn close(&self, code: u32, reason: &[u8]) {
        self.shared().close_local(code, reason);
    }

    /// Wait for the connection to end, and say why.
    pub async fn closed(&self) -> ConnectionError {
        let mut rx = self.shared().closed.subscribe();
        let result = rx.wait_for(Option::is_some).await;
        match result {
            Ok(error) => error.clone().expect("waited for Some"),
            // The sender lives in `Shared`, which we hold.
            Err(_) => ConnectionError::LocallyClosed,
        }
    }

    pub fn close_reason(&self) -> Option<ConnectionError> {
        self.shared().state().error.clone()
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.shared().remote
    }

    pub fn stable_id(&self) -> usize {
        self.shared().stable_id
    }
}

fn io_error(error: &ConnectionError) -> io::Error {
    let kind = match error {
        ConnectionError::TimedOut => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::ConnectionAborted,
    };
    io::Error::new(kind, error.clone())
}

/// Tried to finish or reset a stream that was already finished or reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stream already finished or reset")]
pub struct ClosedStream;

/// The writing half of a stream.
pub struct SendStream {
    guard: Arc<Guard>,
    id: u32,
}

impl SendStream {
    fn end(&mut self, frame: Bytes) -> Result<(), ClosedStream> {
        let shared = &self.guard.shared;
        let mut state = shared.state();
        let send = state
            .streams
            .get_mut(&self.id)
            .and_then(|s| s.send.as_mut())
            .ok_or(ClosedStream)?;
        if send.done {
            return Err(ClosedStream);
        }
        send.done = true;
        if state.error.is_none() {
            let _ = shared.queue(self.id).send(frame);
        }
        Ok(())
    }

    /// No more data: the reader sees end of stream after what was written.
    pub fn finish(&mut self) -> Result<(), ClosedStream> {
        self.end(encode(FIN, self.id, &[]))
    }

    /// Abandon the stream: the reader gets an error instead of end of stream.
    pub fn reset(&mut self, code: u32) -> Result<(), ClosedStream> {
        self.end(encode_code(RESET, self.id, code))
    }
}

impl AsyncWrite for SendStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let shared = &self.guard.shared;
        let mut state = shared.state();
        if let Some(e) = &state.error {
            return Poll::Ready(Err(io_error(e)));
        }
        let Some(send) = state
            .streams
            .get_mut(&self.id)
            .and_then(|s| s.send.as_mut())
        else {
            return Poll::Ready(Err(io::ErrorKind::NotConnected.into()));
        };
        if let Some(code) = send.stopped {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("stream stopped by peer (code {code})"),
            )));
        }
        if send.done {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream already finished",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if send.credit == 0 {
            send.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf
            .len()
            .min(MAX_CHUNK)
            .min(usize::try_from(send.credit).unwrap_or(usize::MAX));
        send.credit -= n as u64;
        let _ = shared.queue(self.id).send(encode(DATA, self.id, &buf[..n]));
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Written data is already queued for the socket.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _ = self.finish();
        Poll::Ready(Ok(()))
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        let _ = self.finish();
        let mut state = self.guard.shared.state();
        if let Some(stream) = state.streams.get_mut(&self.id) {
            stream.send = None;
        }
        state.reap(self.id);
    }
}

/// The reading half of a stream.
pub struct RecvStream {
    guard: Arc<Guard>,
    id: u32,
}

impl RecvStream {
    /// Stop reading: discard what is buffered and make the writer's
    /// further writes fail.
    pub fn stop(&mut self, code: u32) {
        let shared = &self.guard.shared;
        let mut state = shared.state();
        let error = state.error.is_some();
        if let Some(stream) = state.streams.get_mut(&self.id) {
            if let Some(recv) = stream.recv.take() {
                if !error && !recv.fin && recv.reset.is_none() {
                    let _ = shared.urgent.send(encode_code(STOP, self.id, code));
                }
            }
        }
        state.reap(self.id);
    }
}

impl AsyncRead for RecvStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let shared = &self.guard.shared;
        let mut state = shared.state();
        let error = state.error.clone();
        let Some(recv) = state
            .streams
            .get_mut(&self.id)
            .and_then(|s| s.recv.as_mut())
        else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "stream stopped",
            )));
        };
        // Deliver what arrived before any error.
        if let Some(chunk) = recv.buf.front_mut() {
            let n = chunk.len().min(buf.remaining());
            buf.put_slice(&chunk.split_to(n));
            if chunk.is_empty() {
                recv.buf.pop_front();
            }
            recv.buffered -= n;
            recv.consumed += n as u32;
            if recv.consumed >= CREDIT_BATCH && !recv.fin && error.is_none() {
                let credit = std::mem::take(&mut recv.consumed);
                let _ = shared.urgent.send(encode_code(WINDOW, self.id, credit));
            }
            return Poll::Ready(Ok(()));
        }
        if let Some(code) = recv.reset {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("stream reset by peer (code {code})"),
            )));
        }
        if recv.fin {
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = &error {
            return Poll::Ready(Err(io_error(e)));
        }
        recv.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        self.stop(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn pair() -> (Connection, Connection) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let server = tokio::spawn(async move { tokio_tungstenite::accept_async(b).await.unwrap() });
        let (client, _) = tokio_tungstenite::client_async("ws://localhost/rmm", a)
            .await
            .unwrap();
        let server = server.await.unwrap();
        (
            spawn(client, Side::Client, addr),
            spawn(server, Side::Server, addr),
        )
    }

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::OpenBi(0),
            Frame::OpenUni(7),
            Frame::Data(3, Bytes::from_static(b"hello")),
            Frame::Data(3, Bytes::new()),
            Frame::Fin(9),
            Frame::Reset(2, 42),
            Frame::Stop(2, 1),
            Frame::Window(4, 65536),
            Frame::Close(5, "the user ended the session".into()),
        ];
        for frame in frames {
            let bytes = match &frame {
                Frame::OpenBi(id) => encode(OPEN_BI, *id, &[]),
                Frame::OpenUni(id) => encode(OPEN_UNI, *id, &[]),
                Frame::Data(id, b) => encode(DATA, *id, b),
                Frame::Fin(id) => encode(FIN, *id, &[]),
                Frame::Reset(id, c) => encode_code(RESET, *id, *c),
                Frame::Stop(id, c) => encode_code(STOP, *id, *c),
                Frame::Window(id, c) => encode_code(WINDOW, *id, *c),
                Frame::Close(c, r) => {
                    let mut p = c.to_be_bytes().to_vec();
                    p.extend_from_slice(r.as_bytes());
                    encode(CLOSE, 0, &p)
                }
            };
            assert_eq!(decode(bytes), Some(frame));
        }
        assert_eq!(decode(Bytes::from_static(&[DATA, 0, 0])), None);
        assert_eq!(decode(Bytes::from_static(&[99, 0, 0, 0, 1])), None);
        assert_eq!(decode(Bytes::from_static(&[STOP, 0, 0, 0, 1, 0])), None);
    }

    #[tokio::test]
    async fn bidirectional_streams_carry_data_both_ways_and_end_cleanly() {
        let (client, server) = pair().await;
        let (mut send, mut recv) = client.open_bi().await.unwrap();
        send.write_all(b"ping").await.unwrap();
        send.finish().unwrap();

        let (mut s_send, mut s_recv) = server.accept_bi().await.unwrap();
        let mut got = Vec::new();
        s_recv.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"ping");
        s_send.write_all(b"pong").await.unwrap();
        drop(s_send); // implicit finish

        let mut got = Vec::new();
        recv.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"pong");
        assert_eq!(send.finish(), Err(ClosedStream));
    }

    #[tokio::test]
    async fn unidirectional_streams_flow_one_way_in_either_direction() {
        let (client, server) = pair().await;
        let mut up = client.open_uni().await.unwrap();
        let mut down = server.open_uni().await.unwrap();
        up.write_all(b"from client").await.unwrap();
        down.write_all(b"from server").await.unwrap();
        drop((up, down));
        let mut got = String::new();
        server
            .accept_uni()
            .await
            .unwrap()
            .read_to_string(&mut got)
            .await
            .unwrap();
        assert_eq!(got, "from client");
        got.clear();
        client
            .accept_uni()
            .await
            .unwrap()
            .read_to_string(&mut got)
            .await
            .unwrap();
        assert_eq!(got, "from server");
    }

    #[tokio::test]
    async fn a_large_transfer_is_flow_controlled_and_arrives_intact() {
        let (client, server) = pair().await;
        let data: Vec<u8> = (0..(3 * STREAM_WINDOW as usize + 12_345))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let expected = data.clone();
        let writer = tokio::spawn(async move {
            let mut send = client.open_uni().await.unwrap();
            send.write_all(&data).await.unwrap();
            send.finish().unwrap();
            client
        });
        let mut recv = server.accept_uni().await.unwrap();
        let mut got = Vec::new();
        recv.read_to_end(&mut got).await.unwrap();
        assert_eq!(got.len(), expected.len());
        assert!(got == expected);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn a_stalled_reader_blocks_only_its_own_stream() {
        let (client, server) = pair().await;
        let (mut control, _control_rx) = client.open_bi().await.unwrap();
        let mut bulk = client.open_uni().await.unwrap();
        // Nobody reads the bulk stream: the writer stops at the window.
        let big = vec![7u8; STREAM_WINDOW as usize + 1];
        let blocked = tokio::time::timeout(Duration::from_millis(300), bulk.write_all(&big)).await;
        assert!(blocked.is_err(), "writer should be out of credit");

        // The control stream still flows.
        control.write_all(b"heartbeat").await.unwrap();
        let (_s, mut s_control) = server.accept_bi().await.unwrap();
        let mut buf = [0u8; 9];
        s_control.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"heartbeat");

        // Reading the bulk stream returns credit and unblocks the writer.
        let mut s_bulk = server.accept_uni().await.unwrap();
        let reader = tokio::spawn(async move {
            let mut n = 0;
            let mut chunk = vec![0u8; 8192];
            while n < STREAM_WINDOW as usize * 2 {
                n += s_bulk.read(&mut chunk).await.unwrap();
            }
            (n, s_bulk) // still open: dropping it would stop the writer
        });
        bulk.write_all(&big).await.unwrap();
        assert!(reader.await.unwrap().0 >= STREAM_WINDOW as usize * 2);
    }

    #[tokio::test]
    async fn reset_and_stop_surface_as_errors_on_the_other_side() {
        let (client, server) = pair().await;
        let (mut send, _recv) = client.open_bi().await.unwrap();
        send.write_all(b"partial").await.unwrap();
        send.reset(9).unwrap();
        let (mut s_send, mut s_recv) = server.accept_bi().await.unwrap();
        let mut buf = Vec::new();
        let err = s_recv.read_to_end(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);

        // The client drops its reading half: the server's writes start failing.
        drop(_recv);
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Err(e) = s_send.write_all(&[0u8; 1024]).await {
                    return e;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writes should fail once stopped");
        assert_eq!(result.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn close_reaches_the_peer_with_code_and_reason() {
        let (client, server) = pair().await;
        let (_send, mut recv) = client.open_bi().await.unwrap();
        let (_s_send, _s_recv) = server.accept_bi().await.unwrap();
        server.close(5, b"the user ended the session");

        assert_eq!(
            client.closed().await,
            ConnectionError::ApplicationClosed {
                code: 5,
                reason: "the user ended the session".into()
            }
        );
        let mut buf = [0u8; 1];
        assert!(recv.read(&mut buf).await.is_err());
        assert!(client.open_bi().await.is_err());
        assert_eq!(server.close_reason(), Some(ConnectionError::LocallyClosed));
    }

    #[tokio::test]
    async fn data_written_before_a_graceful_finish_survives_the_close() {
        // The enrollment reply pattern: write, finish, wait for the peer to
        // hang up.
        let (client, server) = pair().await;
        let (mut send, mut recv) = client.open_bi().await.unwrap();
        send.write_all(b"enroll").await.unwrap();
        let (mut s_send, mut s_recv) = server.accept_bi().await.unwrap();
        let mut buf = [0u8; 6];
        s_recv.read_exact(&mut buf).await.unwrap();
        s_send.write_all(b"certificate").await.unwrap();
        s_send.finish().unwrap();
        let mut got = Vec::new();
        recv.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"certificate");
        client.close(0, b"enrolled");
        assert!(matches!(
            server.closed().await,
            ConnectionError::ApplicationClosed { code: 0, .. }
        ));
    }

    #[tokio::test]
    async fn dropping_every_handle_closes_the_connection() {
        let (client, server) = pair().await;
        let (send, recv) = client.open_bi().await.unwrap();
        drop(client);
        // Streams keep it alive...
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(server.close_reason(), None);
        drop((send, recv));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), server.closed())
                .await
                .unwrap(),
            ConnectionError::ApplicationClosed { code: 0, .. }
        ));
    }

    #[tokio::test]
    async fn a_silent_peer_times_out() {
        tokio::time::pause();
        let (a, _b) = tokio::io::duplex(1024);
        // A WebSocket whose peer never says anything (not even a handshake
        // reply is needed: build it from the raw socket).
        let ws = WebSocketStream::from_raw_socket(
            a,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let conn = spawn(ws, Side::Client, "127.0.0.1:1".parse().unwrap());
        let closed = conn.closed();
        tokio::pin!(closed);
        tokio::time::advance(IDLE_TIMEOUT + Duration::from_secs(1)).await;
        assert_eq!(closed.await, ConnectionError::TimedOut);
    }

    #[tokio::test]
    async fn frames_really_are_framed_postcard_messages_end_to_end() {
        // What the RMM does with a stream: length-delimited messages.
        let (client, server) = pair().await;
        let (mut send, _recv) = client.open_bi().await.unwrap();
        let msg = protocol::Message::Hello {
            agent_id: "agt-1".into(),
            version: protocol::PROTOCOL_VERSION,
        };
        protocol::write_frame(&mut send, &msg).await.unwrap();
        let (_s, mut s_recv) = server.accept_bi().await.unwrap();
        let got: Option<protocol::Message> = protocol::read_frame(&mut s_recv).await.unwrap();
        assert_eq!(got, Some(msg));
    }
}
