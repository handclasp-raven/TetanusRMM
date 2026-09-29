//! The viewer's connection to the server.

use std::net::SocketAddr;

use protocol::clipboard::ClipboardData;
use protocol::input::InputEvent;
use protocol::media::{MediaFrame, MonitorInfo};
use protocol::{read_frame, write_frame, FrameError, Message, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tracing::debug;
use transport::{Connection, ConnectionError, Preference, TransportSettings};

pub struct ViewerOptions {
    /// Server QUIC (UDP) address.
    pub server: SocketAddr,
    /// Which transports to use, and where the WebSocket fallback is.
    pub transport: TransportSettings,
    /// Name the server certificate must be valid for.
    pub server_name: String,
    /// PEM CA the server certificate must chain to.
    pub ca_pem: String,
    /// Viewer-session token from `POST /api/agents/{id}/viewer-sessions`.
    pub token: String,
    pub bind: Option<SocketAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Tls(#[from] common::TlsError),
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The server closed the connection, with its reason.
    #[error("server refused: {0}")]
    Refused(String),
    #[error("unexpected reply from server: {0:?}")]
    Unexpected(Box<Message>),
}

/// What the server sent after accepting the token.
#[derive(Debug, Clone)]
pub struct Welcome {
    pub agent_id: String,
    pub monitors: Vec<MonitorInfo>,
    pub active_monitor: Option<u32>,
}

#[derive(Debug)]
pub enum ViewerEvent {
    Monitors(Vec<MonitorInfo>),
    /// The stream now shows this monitor.
    StreamMonitor(u32),
    Frame(MediaFrame),
    /// The remote user's clipboard changed.
    Clipboard(ClipboardData),
    /// The connection ended; the reason is human-readable.
    Closed(String),
}

/// Handle for sending requests to the server.
#[derive(Clone)]
pub struct ViewerHandle {
    commands: mpsc::UnboundedSender<Message>,
    connection: Connection,
}

impl ViewerHandle {
    /// Ask for a different monitor (applies to everyone watching this agent).
    pub fn select_monitor(&self, monitor: u32) {
        let _ = self.commands.send(Message::SelectMonitor { monitor });
    }

    /// Ask for a keyframe, e.g. after a decode error.
    pub fn request_keyframe(&self) {
        let _ = self.commands.send(Message::RequestKeyframe);
    }

    /// Inject input on the remote machine.
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.commands.send(Message::Input(event));
    }

    /// Put `data` on the remote clipboard.
    pub fn send_clipboard(&self, data: ClipboardData) {
        let _ = self.commands.send(Message::Clipboard(data));
    }

    pub fn close(&self) {
        self.connection
            .close(protocol::close_code::NORMAL, b"viewer closed");
    }

    /// Which transport the session runs on.
    pub fn transport(&self) -> transport::TransportKind {
        self.connection.kind()
    }
}

/// Frames buffered between network and decoder.
const EVENT_QUEUE: usize = 64;

/// How often to acknowledge frames, once the server asks (adaptive
/// bitrate; the server's `relay::delay` allows for this interval).
pub const ACK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The server's reason for closing. The stream can end a moment before the
/// connection close (which carries the reason) arrives, so wait briefly.
async fn close_reason(conn: &Connection, fallback: impl std::fmt::Display) -> String {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), conn.closed()).await;
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed { reason, .. }) => reason,
        Some(other) => other.to_string(),
        None => fallback.to_string(),
    }
}

/// Progress reported before the session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// The remote user is being asked to accept, for up to `timeout_secs`.
    WaitingForConsent { timeout_secs: u32 },
}

/// Connect, authenticate with the token, and start receiving.
pub async fn connect(
    options: &ViewerOptions,
) -> Result<(Welcome, ViewerHandle, mpsc::Receiver<ViewerEvent>), ClientError> {
    connect_with_status(options, |_| {}).await
}

/// [`connect`], reporting progress (such as waiting for the remote user's
/// consent) to `status`. A refused session is [`ClientError::Refused`] with
/// the server's reason.
pub async fn connect_with_status(
    options: &ViewerOptions,
    mut status: impl FnMut(Pending),
) -> Result<(Welcome, ViewerHandle, mpsc::Receiver<ViewerEvent>), ClientError> {
    let ca = common::tls::certs_from_pem(&options.ca_pem)?;
    let target = options
        .transport
        .target(options.server, &options.server_name, options.bind);
    // No client certificate: the token is the credential. The token is
    // only spent once connected, so falling back cannot waste it.
    let dialer = transport::Dialer::new(target, &ca, None)?;
    let dialed = dialer
        .dial(&mut Preference::default())
        .await
        .map_err(|e| match e {
            transport::dial::DialError::Tls(e) => ClientError::Tls(e),
            other => ClientError::Unreachable(other.to_string()),
        })?;
    let (conn, endpoint) = (dialed.connection, dialed.endpoint);

    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::ViewerHello {
            token: options.token.clone(),
            version: PROTOCOL_VERSION,
        },
    )
    .await?;
    let welcome = loop {
        match read_frame::<_, Message>(&mut recv).await {
            Ok(Some(Message::ConsentPending { timeout_secs })) => {
                status(Pending::WaitingForConsent { timeout_secs });
            }
            Ok(Some(Message::ViewerWelcome {
                agent_id,
                monitors,
                active_monitor,
            })) => {
                break Welcome {
                    agent_id,
                    monitors,
                    active_monitor,
                }
            }
            Ok(Some(other)) => return Err(ClientError::Unexpected(Box::new(other))),
            Ok(None) => {
                return Err(ClientError::Refused(
                    close_reason(&conn, "connection closed").await,
                ))
            }
            Err(e) => return Err(ClientError::Refused(close_reason(&conn, e).await)),
        }
    };

    let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
    let (commands, mut outbox) = mpsc::unbounded_channel::<Message>();
    let handle = ViewerHandle {
        commands,
        connection: conn.clone(),
    };

    // Newest frame handed to the app (0 = none yet), and whether the
    // server wants acknowledgements.
    let received = std::sync::atomic::AtomicU64::new(0);
    let acks_enabled = std::sync::atomic::AtomicBool::new(false);
    let ack_commands = handle.commands.clone();
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let writer = async {
            while let Some(msg) = outbox.recv().await {
                write_frame(&mut send, &msg).await?;
            }
            Ok::<(), ClientError>(())
        };
        let control = async {
            while let Some(msg) = read_frame::<_, Message>(&mut recv).await? {
                let event = match msg {
                    Message::EnableFrameAcks => {
                        acks_enabled.store(true, Ordering::Relaxed);
                        continue;
                    }
                    Message::MonitorList { monitors } => ViewerEvent::Monitors(monitors),
                    Message::StreamMonitor { monitor } => ViewerEvent::StreamMonitor(monitor),
                    Message::Clipboard(data) => ViewerEvent::Clipboard(data),
                    other => {
                        debug!(?other, "ignoring control message");
                        continue;
                    }
                };
                if events_tx.send(event).await.is_err() {
                    break;
                }
            }
            Ok(())
        };
        let video = async {
            let mut stream = conn.accept_uni().await?;
            while let Some(frame) = read_frame::<_, MediaFrame>(&mut stream).await? {
                let seq = frame.seq;
                // Blocks while the app is behind: then acknowledgements
                // stall too, and the server sees this viewer lagging.
                if events_tx.send(ViewerEvent::Frame(frame)).await.is_err() {
                    break;
                }
                received.store(seq + 1, Ordering::Relaxed);
            }
            Ok(())
        };
        let acks = async {
            let mut ticker = tokio::time::interval(ACK_INTERVAL);
            let mut acked = 0;
            loop {
                ticker.tick().await;
                let newest = received.load(Ordering::Relaxed);
                if newest != acked && acks_enabled.load(Ordering::Relaxed) {
                    acked = newest;
                    let _ = ack_commands.send(Message::FrameAck { seq: newest - 1 });
                }
            }
        };
        let result = tokio::select! {
            r = writer => r,
            r = control => r,
            r = video => r,
            () = acks => unreachable!("acknowledging never ends"),
        };
        let reason = match result {
            Ok(()) => close_reason(&conn, "stream ended").await,
            Err(e) => close_reason(&conn, e).await,
        };
        let _ = events_tx.send(ViewerEvent::Closed(reason)).await;
        conn.close(protocol::close_code::NORMAL, b"");
        if let Some(endpoint) = endpoint {
            endpoint.close(protocol::close_code::NORMAL.into(), b"");
        }
    });

    Ok((welcome, handle, events))
}
