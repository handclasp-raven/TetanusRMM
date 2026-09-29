//! The viewer's connection to the server.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use protocol::clipboard::ClipboardData;
use protocol::input::InputEvent;
use protocol::media::{MediaFrame, MonitorInfo};
use protocol::{read_frame, write_frame, FrameError, Message, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use tracing::debug;

pub struct ViewerOptions {
    pub server: SocketAddr,
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
    #[error("binding local UDP socket: {0}")]
    Bind(#[source] std::io::Error),
    #[error(transparent)]
    Connect(#[from] quinn::ConnectError),
    #[error(transparent)]
    Connection(#[from] quinn::ConnectionError),
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
    connection: quinn::Connection,
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
            .close(protocol::close_code::NORMAL.into(), b"viewer closed");
    }
}

/// Frames buffered between network and decoder.
const EVENT_QUEUE: usize = 64;

/// The server's reason for closing. The stream can end a moment before the
/// connection close (which carries the reason) arrives, so wait briefly.
async fn close_reason(conn: &quinn::Connection, fallback: impl std::fmt::Display) -> String {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), conn.closed()).await;
    match conn.close_reason() {
        Some(quinn::ConnectionError::ApplicationClosed(close)) => {
            String::from_utf8_lossy(&close.reason).into_owned()
        }
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
    let bind = options.bind.unwrap_or(match options.server {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    });
    let mut endpoint = quinn::Endpoint::client(bind).map_err(ClientError::Bind)?;
    // No client certificate: the token is the credential.
    endpoint.set_default_client_config(common::quic::client_config(&ca, None)?);
    let conn = endpoint
        .connect(options.server, &options.server_name)?
        .await?;

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

    tokio::spawn(async move {
        let writer = async {
            while let Some(msg) = outbox.recv().await {
                write_frame(&mut send, &msg).await?;
            }
            Ok::<(), ClientError>(())
        };
        let control = async {
            while let Some(msg) = read_frame::<_, Message>(&mut recv).await? {
                let event = match msg {
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
                if events_tx.send(ViewerEvent::Frame(frame)).await.is_err() {
                    break;
                }
            }
            Ok(())
        };
        let result = tokio::select! {
            r = writer => r,
            r = control => r,
            r = video => r,
        };
        let reason = match result {
            Ok(()) => close_reason(&conn, "stream ended").await,
            Err(e) => close_reason(&conn, e).await,
        };
        let _ = events_tx.send(ViewerEvent::Closed(reason)).await;
        endpoint.close(protocol::close_code::NORMAL.into(), b"");
    });

    Ok((welcome, handle, events))
}
