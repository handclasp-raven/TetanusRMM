//! Remote desktop viewer. Connects to the server (not the agent) with a
//! short-lived viewer-session token, shows the agent's screen, and sends
//! mouse and keyboard input and clipboard changes to it.
//!
//! Around the picture: a toolbar (display mode, monitor, refresh, full
//! screen, the side panel, disconnect) and a side panel with the agent's
//! status, command buttons and file transfer (see `viewer::ui`). The panel
//! uses the server's HTTPS API as the signed-in technician; the TUI passes
//! the API address and its session token (`RMM_API_TOKEN`, never on the
//! command line) when it starts the viewer.
//!
//! Every key goes to the remote machine except these viewer shortcuts, all
//! with Ctrl+Alt+Shift held: M opens the monitor menu, 1-9 picks a monitor,
//! P shows or hides the panel, F toggles full screen, - and + make the
//! controls' text smaller or larger, F5 asks for a fresh keyframe, Q quits. While a menu or dialog is open, keys go to it (Esc
//! closes it; in the monitor menu 1-9 pick).
//!
//! The FPS menu sets how many frames a second this technician wants (the
//! agent streams at the fastest any technician watching wants); "Auto"
//! lets the agent choose by how fast the network carries the video. At
//! "Original size" a remote screen larger than the window is panned by
//! holding the pointer near an edge.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::Parser;
use protocol::input::{normalise, InputEvent, MouseButton, WHEEL_NOTCH};
use protocol::media::{FrameRate, MonitorInfo, StreamStatus};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};
use viewer::api::{remote_file_name, AgentDetails, ApiClient};
use viewer::client::{self, Pending, ViewerEvent, ViewerHandle, ViewerOptions, Welcome};
use viewer::decode::{Picture, VideoDecoder};
use viewer::keymap;
use viewer::render::{self, DisplayMode, Rect, View};
use viewer::ui::{self, Action, Chrome, Hit, Metrics, Notice, Overlay, QuickCommand};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

/// How often the side panel's status is refreshed.
const STATUS_INTERVAL: Duration = Duration::from_secs(5);
/// How long notices stay up.
const NOTICE_TIME: Duration = Duration::from_secs(6);
const ERROR_NOTICE_TIME: Duration = Duration::from_secs(12);
/// Most notices shown at once (the oldest go first).
const MAX_NOTICES: usize = 4;
/// Original size: how close to an edge (in text heights) the pointer pans,
/// and how fast at the very edge (logical pixels a second).
const PAN_ZONE_CHARS: f64 = 3.0;
const PAN_SPEED: f64 = 1500.0;
/// Pan steps while the pointer is in an edge zone.
const PAN_TICK: Duration = Duration::from_millis(16);

#[derive(Parser)]
#[command(about = "RMM remote desktop viewer", version)]
struct Cli {
    /// Server QUIC address.
    #[arg(long, env = "RMM_SERVER", default_value = "127.0.0.1:4433")]
    server: SocketAddr,
    /// Name the server certificate must be valid for.
    #[arg(long, env = "RMM_SERVER_NAME", default_value = "localhost")]
    server_name: String,
    /// PEM CA certificate the server's certificate chains to.
    #[arg(long, env = "RMM_SERVER_CA")]
    ca: PathBuf,
    /// auto (QUIC, falling back to WebSocket over TLS when UDP is blocked),
    /// quic, or websocket.
    #[arg(long, env = "RMM_TRANSPORT", default_value_t = transport::TransportMode::Auto)]
    transport: transport::TransportMode,
    /// TCP address of the server's WebSocket fallback, if it is not the
    /// --server address.
    #[arg(long, env = "RMM_WS_SERVER")]
    ws_server: Option<SocketAddr>,
    /// Viewer-session token (from POST /api/agents/{id}/viewer-sessions).
    #[arg(long, env = "RMM_VIEWER_TOKEN", hide_env_values = true)]
    token: String,
    /// Never connect straight to the agent: keep the session on the server's
    /// relay. (Sessions are end-to-end encrypted either way.)
    #[arg(long, env = "RMM_NO_DIRECT")]
    no_direct: bool,
    /// Switch to this monitor id after connecting.
    #[arg(long)]
    monitor: Option<u32>,
    /// The server's HTTPS API (e.g. https://rmm.example.com:8443), for the
    /// side panel. Also needs the session token in RMM_API_TOKEN.
    #[arg(long, env = "RMM_API_URL")]
    api_url: Option<String>,
    /// A command button for the side panel, as LABEL=COMMAND (e.g.
    /// "Network connections=ncpa.cpl"). Repeat for more. Without any, the
    /// panel offers cmd, ncpa.cpl and mstsc.
    #[arg(long = "command", value_name = "LABEL=COMMAND")]
    commands: Vec<QuickCommand>,
    /// No command buttons, rather than the defaults, when --command is not
    /// given.
    #[arg(long)]
    no_default_commands: bool,
    /// How the picture fills its area: scale (fit, keeping its shape),
    /// stretch, fill (keeping its shape, cropping the edges), or original
    /// (one remote pixel per screen pixel, panned at the edges).
    #[arg(long, default_value = "scale")]
    display: DisplayMode,
    /// Frames a second to ask for: auto (the agent picks by network
    /// speed), max, or a number such as 60. Also in the toolbar's FPS menu.
    #[arg(long, env = "RMM_VIEWER_FPS", default_value = "auto")]
    fps: FrameRate,
    /// Start with the side panel hidden.
    #[arg(long)]
    no_panel: bool,
    /// Text size of the toolbar, panel, menus and dialogs, in pixels
    /// (before the display's scaling). Also in the toolbar's Text menu.
    #[arg(long, env = "RMM_VIEWER_FONT_SIZE", default_value_t = ui::DEFAULT_FONT_PX,
          value_parser = clap::value_parser!(u32).range(i64::from(ui::MIN_FONT_PX)..=i64::from(ui::MAX_FONT_PX)))]
    font_size: u32,
    /// Save the text size here whenever it is changed in the viewer, as
    /// `{"font_size": N}`, so whoever starts the next viewer (the TUI) can
    /// start it at that size.
    #[arg(long, value_name = "FILE")]
    remember_font_size: Option<PathBuf>,
    /// Headless: decode frames, write the last one to this PPM file, exit.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// With --snapshot: how many frames to decode first.
    #[arg(long, default_value_t = 30)]
    frames: u32,
}

fn main() -> anyhow::Result<()> {
    common::logging::init();
    let cli = Cli::parse();
    let ca_pem = std::fs::read_to_string(&cli.ca)
        .with_context(|| format!("reading {}", cli.ca.display()))?;
    let options = ViewerOptions {
        server: cli.server,
        transport: transport::TransportSettings {
            mode: cli.transport,
            ws_addr: cli.ws_server,
        },
        server_name: cli.server_name.clone(),
        ca_pem: ca_pem.clone(),
        token: cli.token.clone(),
        bind: None,
        direct: peer::DirectSettings {
            enabled: !cli.no_direct,
            ..Default::default()
        },
    };
    if let Some(path) = &cli.snapshot {
        return snapshot(options, cli.monitor, cli.fps, cli.frames, path);
    }
    // The token only ever comes from the environment: the command line is
    // visible to other local users.
    let api_token = std::env::var("RMM_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let api = match (&cli.api_url, api_token) {
        (Some(url), Some(token)) => Some(ApiClient::new(url, token, &ca_pem)?),
        (Some(_), None) => {
            warn!("--api-url given without RMM_API_TOKEN: the side panel stays empty");
            None
        }
        _ => None,
    };
    let commands = if cli.commands.is_empty() && !cli.no_default_commands {
        ui::default_commands()
    } else {
        cli.commands
    };
    let panel = PanelSetup {
        api,
        commands,
        display: cli.display,
        frame_rate: cli.fps,
        show_panel: !cli.no_panel,
        font_px: cli.font_size,
        font_file: cli.remember_font_size,
    };
    windowed(options, cli.monitor, panel)
}

// ---------------------------------------------------------------------------
// Headless snapshot mode.

fn snapshot(
    options: ViewerOptions,
    monitor: Option<u32>,
    frame_rate: FrameRate,
    frames: u32,
    path: &Path,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (welcome, handle, mut events) = client::connect(&options).await?;
        info!(agent = %welcome.agent_id, monitors = welcome.monitors.len(), "connected");
        for m in &welcome.monitors {
            info!(
                "monitor {}: {} {}x{}{}",
                m.id,
                m.name,
                m.width,
                m.height,
                if m.primary { " (primary)" } else { "" }
            );
        }
        if let Some(m) = monitor {
            handle.select_monitor(m);
        }
        handle.set_frame_rate(frame_rate);
        let mut decoder = VideoDecoder::new()?;
        let mut decoded = 0;
        let mut last: Option<Picture> = None;
        let start = Instant::now();
        while decoded < frames {
            let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .context("no frames for 20 s")?;
            match event {
                Some(ViewerEvent::Frame(frame)) => match decoder.decode(&frame) {
                    // After a switch, ignore pictures from the old monitor.
                    Ok(Some(p)) if monitor.is_none_or(|m| m == p.monitor) => {
                        decoded += 1;
                        last = Some(p);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("{e}; requesting keyframe");
                        handle.request_keyframe();
                    }
                },
                Some(ViewerEvent::StreamStatus(status)) => info!(
                    frame_rate = %status.frame_rate,
                    fps = status.fps,
                    bitrate = status.bitrate,
                    "stream status"
                ),
                Some(ViewerEvent::Closed(reason)) => bail!("connection closed: {reason}"),
                None => bail!("connection closed"),
                Some(_) => {}
            }
        }
        let p = last.expect("decoded at least one frame");
        write_ppm(path, &p)?;
        info!(
            path = %path.display(),
            size = %format_args!("{}x{}", p.width, p.height),
            monitor = p.monitor,
            decoded,
            secs = format_args!("{:.1}", start.elapsed().as_secs_f64()),
            "snapshot written"
        );
        handle.close();
        Ok(())
    })
}

fn write_ppm(path: &Path, p: &Picture) -> std::io::Result<()> {
    let mut out = format!("P6\n{} {}\n255\n", p.width, p.height).into_bytes();
    for px in &p.pixels {
        out.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
    }
    std::fs::write(path, out)
}

// ---------------------------------------------------------------------------
// Windowed mode.

/// What the window starts with besides the session itself.
struct PanelSetup {
    api: Option<ApiClient>,
    commands: Vec<QuickCommand>,
    display: DisplayMode,
    frame_rate: FrameRate,
    show_panel: bool,
    font_px: u32,
    font_file: Option<PathBuf>,
}

enum UserEvent {
    /// Progress before the session starts (e.g. waiting for consent).
    Status(String),
    Connected(Welcome, ViewerHandle),
    /// A new picture is in the shared slot.
    Picture,
    Monitors(Vec<MonitorInfo>),
    StreamMonitor(u32),
    StreamStatus(StreamStatus),
    Path(protocol::e2e::Path),
    Closed(String),
    /// The agent's status for the side panel, or why it could not be had.
    Details(Result<Box<AgentDetails>, String>),
    Notice {
        text: String,
        error: bool,
    },
    /// A file transfer's progress, or `None` when it is over.
    Busy(Option<String>),
    /// The file the technician picked to upload.
    UploadPicked(PathBuf),
    /// The upload's target already exists: ask before replacing it.
    UploadExists {
        local: PathBuf,
        remote: String,
    },
}

/// Work for the API worker (see [`api_worker`]).
enum Job {
    Refresh,
    Launch(QuickCommand),
    /// Ask for a local file to upload.
    PickUpload,
    Upload {
        local: PathBuf,
        remote: String,
        overwrite: bool,
    },
    /// Ask where to save `remote`, then download it.
    Download {
        remote: String,
    },
}

/// The newest decoded picture. The decoder overwrites it; the window takes
/// it. A slow window skips pictures instead of queueing them.
type Slot = Arc<Mutex<Option<Picture>>>;

fn windowed(options: ViewerOptions, monitor: Option<u32>, setup: PanelSetup) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let slot: Slot = Arc::new(Mutex::new(None));
    let (jobs_tx, jobs_rx) = tokio::sync::mpsc::unbounded_channel();
    let api = setup.api;
    let has_api = api.is_some();
    let frame_rate = setup.frame_rate;
    std::thread::spawn({
        let slot = slot.clone();
        move || network(options, monitor, frame_rate, proxy, slot, api, jobs_rx)
    });
    let mut app = App {
        slot,
        window: None,
        size: (0, 0),
        metrics: Metrics::new(setup.font_px, 1.0),
        font_px: setup.font_px,
        font_file: setup.font_file,
        scale_factor: 1.0,
        handle: None,
        agent_id: None,
        monitors: Vec::new(),
        active: None,
        picture: None,
        modifiers: ModifiersState::empty(),
        held_keys: BTreeSet::new(),
        held_buttons: Vec::new(),
        last_move: None,
        status: Some("Connecting...".into()),
        path: protocol::e2e::Path::Relayed,
        frames: 0,
        fps: 0.0,
        fps_since: Instant::now(),
        display: setup.display,
        pan: (0.0, 0.0),
        pan_tick: None,
        frame_rate: setup.frame_rate,
        streamed_fps: None,
        window_title: String::new(),
        show_panel: setup.show_panel,
        fullscreen: false,
        scroll: 0,
        overlay: None,
        notices: Vec::new(),
        cursor: None,
        hover: None,
        api: has_api,
        details: None,
        details_error: None,
        commands: setup.commands,
        busy: None,
        jobs: has_api.then_some(jobs_tx),
        last_remote_dir: None,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Network + decode, off the UI thread; then the API worker for the panel.
fn network(
    options: ViewerOptions,
    monitor: Option<u32>,
    frame_rate: FrameRate,
    proxy: EventLoopProxy<UserEvent>,
    slot: Slot,
    api: Option<ApiClient>,
    jobs: UnboundedReceiver<Job>,
) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        let status = |pending: Pending| {
            let Pending::WaitingForConsent { timeout_secs } = pending;
            let _ = proxy.send_event(UserEvent::Status(format!(
                "Waiting for the remote user to accept (up to {timeout_secs} s)..."
            )));
        };
        let (welcome, handle, mut events) =
            match client::connect_with_status(&options, status).await {
                Ok(connected) => connected,
                Err(e) => {
                    let _ = proxy.send_event(UserEvent::Closed(e.to_string()));
                    return;
                }
            };
        if let Some(m) = monitor {
            handle.select_monitor(m);
        }
        handle.set_frame_rate(frame_rate);
        if let Some(api) = api {
            tokio::spawn(api_worker(
                api,
                welcome.agent_id.clone(),
                jobs,
                proxy.clone(),
            ));
        }
        let _ = proxy.send_event(UserEvent::Connected(welcome, handle.clone()));

        // Clipboard sync polls the local clipboard: its own thread.
        let (clipboard_tx, clipboard_rx) = std::sync::mpsc::channel();
        std::thread::spawn({
            let handle = handle.clone();
            move || viewer::clipboard::run(handle, clipboard_rx)
        });

        // Decoding is CPU work: its own thread, fed through a channel.
        // Every frame must be decoded (each builds on the last), but when
        // several are waiting only the newest is worth converting for
        // display: so a burst (a video playing) never builds up a backlog
        // that would put the picture behind the remote screen.
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(64);
        std::thread::spawn({
            let proxy = proxy.clone();
            let handle = handle.clone();
            move || {
                let mut decoder = VideoDecoder::new().expect("OpenH264 decoder");
                let mut batch = Vec::new();
                while let Some(first) = frames_rx.blocking_recv() {
                    batch.push(first);
                    while let Ok(frame) = frames_rx.try_recv() {
                        batch.push(frame);
                    }
                    let last = batch.len() - 1;
                    for (i, frame) in batch.drain(..).enumerate() {
                        match decoder.decode_as(&frame, i == last) {
                            Ok(Some(picture)) => {
                                let was_empty = slot
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .replace(picture)
                                    .is_none();
                                if was_empty && proxy.send_event(UserEvent::Picture).is_err() {
                                    return;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!("{e}; requesting keyframe");
                                handle.request_keyframe();
                            }
                        }
                    }
                }
            }
        });

        while let Some(event) = events.recv().await {
            let user_event = match event {
                ViewerEvent::Frame(frame) => {
                    if frames_tx.send(frame).await.is_err() {
                        break;
                    }
                    continue;
                }
                ViewerEvent::Clipboard(data) => {
                    let _ = clipboard_tx.send(data);
                    continue;
                }
                ViewerEvent::Monitors(m) => UserEvent::Monitors(m),
                ViewerEvent::StreamMonitor(m) => UserEvent::StreamMonitor(m),
                ViewerEvent::StreamStatus(status) => UserEvent::StreamStatus(status),
                ViewerEvent::Path(path) => UserEvent::Path(path),
                ViewerEvent::Closed(reason) => UserEvent::Closed(reason),
            };
            if proxy.send_event(user_event).is_err() {
                break;
            }
        }
    });
}

/// Keeps the panel's status fresh and runs what its buttons ask for, each
/// job on its own task so a long download never holds up the status.
async fn api_worker(
    api: ApiClient,
    agent_id: String,
    mut jobs: UnboundedReceiver<Job>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let mut ticker = tokio::time::interval(STATUS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let refreshing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    loop {
        let job = tokio::select! {
            _ = ticker.tick() => Job::Refresh,
            job = jobs.recv() => match job {
                Some(job) => job,
                None => return,
            },
        };
        if matches!(job, Job::Refresh) && refreshing.swap(true, Ordering::AcqRel) {
            continue; // the last one is still going
        }
        let (api, agent_id, proxy, refreshing) = (
            api.clone(),
            agent_id.clone(),
            proxy.clone(),
            refreshing.clone(),
        );
        tokio::spawn(async move {
            let is_refresh = matches!(job, Job::Refresh);
            run_job(&api, &agent_id, job, &proxy).await;
            if is_refresh {
                refreshing.store(false, Ordering::Release);
            }
        });
    }
}

async fn run_job(api: &ApiClient, agent_id: &str, job: Job, proxy: &EventLoopProxy<UserEvent>) {
    let send = |event| {
        let _ = proxy.send_event(event);
    };
    let notice = |text: String, error: bool| send(UserEvent::Notice { text, error });
    match job {
        Job::Refresh => {
            let details = api.agent(agent_id).await;
            send(UserEvent::Details(
                details.map(Box::new).map_err(|e| e.message),
            ));
        }
        Job::Launch(command) => match api.launch(agent_id, &command.command).await {
            Ok(user) if user.is_empty() => notice(format!("Started {}.", command.label), false),
            Ok(user) => notice(format!("Started {} as {user}.", command.label), false),
            Err(e) => notice(format!("{}: {}", command.label, e.message), true),
        },
        Job::PickUpload => {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Upload a file to the agent")
                .pick_file()
                .await;
            match picked {
                Some(file) => send(UserEvent::UploadPicked(file.path().to_path_buf())),
                None => send(UserEvent::Busy(None)),
            }
        }
        Job::Upload {
            local,
            remote,
            overwrite,
        } => {
            let name = local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let progress = progress_reporter(proxy.clone(), format!("Uploading {name}"));
            send(UserEvent::Busy(Some(format!("Uploading {name}..."))));
            match api
                .upload(agent_id, &local, &remote, overwrite, progress)
                .await
            {
                Ok(size) => notice(
                    format!("Uploaded {name} ({}) to {remote}.", ui::bytes(size)),
                    false,
                ),
                Err(e) if e.already_exists() && !overwrite => {
                    send(UserEvent::UploadExists { local, remote })
                }
                Err(e) => notice(format!("Upload of {name} failed: {}", e.message), true),
            }
            send(UserEvent::Busy(None));
        }
        Job::Download { remote } => {
            let name = remote_file_name(&remote).to_owned();
            let target = rfd::AsyncFileDialog::new()
                .set_title("Save the agent's file as")
                .set_file_name(&name)
                .save_file()
                .await;
            let Some(target) = target else {
                send(UserEvent::Busy(None));
                return;
            };
            let local = target.path().to_path_buf();
            let progress = progress_reporter(proxy.clone(), format!("Downloading {name}"));
            send(UserEvent::Busy(Some(format!("Downloading {name}..."))));
            match api.download(agent_id, &remote, &local, progress).await {
                Ok(size) => notice(
                    format!(
                        "Downloaded {name} ({}) to {}.",
                        ui::bytes(size),
                        local.display()
                    ),
                    false,
                ),
                Err(e) => notice(format!("Download of {name} failed: {}", e.message), true),
            }
            send(UserEvent::Busy(None));
        }
    }
}

/// A progress callback that tells the window `"{what}: 42%"`, at most once
/// per percent.
fn progress_reporter(
    proxy: EventLoopProxy<UserEvent>,
    what: String,
) -> impl Fn(u64, u64) + Send + Sync + 'static {
    let proxy = Mutex::new(proxy);
    let last = AtomicU64::new(u64::MAX);
    move |done, total| {
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        if last.swap(percent, Ordering::AcqRel) != percent {
            let text = format!("{what}: {percent}%");
            let proxy = proxy.lock().unwrap_or_else(|e| e.into_inner());
            let _ = proxy.send_event(UserEvent::Busy(Some(text)));
        }
    }
}

struct Window2 {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
}

struct App {
    slot: Slot,
    window: Option<Window2>,
    /// Inner size in physical pixels.
    size: (u32, u32),
    metrics: Metrics,
    handle: Option<ViewerHandle>,
    agent_id: Option<String>,
    monitors: Vec<MonitorInfo>,
    active: Option<u32>,
    picture: Option<Picture>,
    modifiers: ModifiersState,
    /// Scancodes and buttons pressed on the remote, released on focus loss.
    held_keys: BTreeSet<u16>,
    held_buttons: Vec<MouseButton>,
    /// Last pointer position sent, to skip duplicates.
    last_move: Option<(u32, u16, u16)>,
    status: Option<String>,
    /// Relayed or direct.
    path: protocol::e2e::Path,
    frames: u32,
    fps: f64,
    fps_since: Instant,

    // The controls around the picture.
    /// Text size as chosen, and the window's scale factor: together they
    /// make `metrics`.
    font_px: u32,
    /// Where to remember a text size picked here (`--remember-font-size`).
    font_file: Option<PathBuf>,
    scale_factor: f64,
    display: DisplayMode,
    /// Original size: the picture pixel at the area's top-left corner
    /// (fractional while panning smoothly).
    pan: (f64, f64),
    /// Last pan step, while the pointer is in an edge zone.
    pan_tick: Option<Instant>,
    /// This technician's frame rate, and what the agent last said it
    /// streams at.
    frame_rate: FrameRate,
    streamed_fps: Option<u32>,
    /// The window title as last set (setting it is not free).
    window_title: String,
    show_panel: bool,
    fullscreen: bool,
    /// How far the panel is scrolled, in pixels.
    scroll: u32,
    overlay: Option<Overlay>,
    notices: Vec<Notice>,
    cursor: Option<(f64, f64)>,
    /// The highlighted control, to redraw only when it changes.
    hover: Option<Rect>,
    /// Whether the panel can reach the API.
    api: bool,
    details: Option<AgentDetails>,
    details_error: Option<String>,
    commands: Vec<QuickCommand>,
    /// A file transfer (or its file dialog) in progress.
    busy: Option<String>,
    jobs: Option<UnboundedSender<Job>>,
    /// Folder of the last path used on the agent, to start the next from.
    last_remote_dir: Option<String>,
}

impl App {
    fn title(&self) -> String {
        let agent = self.agent_id.as_deref().unwrap_or("connecting");
        let monitor = self
            .active
            .and_then(|a| self.monitors.iter().find(|m| m.id == a))
            .map(|m| format!("{} {}x{}", m.name, m.width, m.height))
            .unwrap_or_else(|| "-".into());
        // Sessions are end-to-end encrypted on either path.
        format!(
            "RMM Viewer - {agent} - {monitor} - {:.0} fps - encrypted, {}",
            self.fps, self.path
        )
    }

    fn info(&self) -> String {
        format!("{:.0} fps | encrypted, {}", self.fps, self.path)
    }

    fn layout(&self) -> ui::Layout {
        ui::layout(self.size.0, self.size.1, &self.metrics, self.show_panel)
    }

    fn chrome<'a>(&'a self, info: &'a str) -> Chrome<'a> {
        Chrome {
            metrics: self.metrics,
            layout: self.layout(),
            toolbar: ui::Toolbar {
                display: self.display,
                frame_rate: self.frame_rate,
                streamed_fps: self.streamed_fps,
                monitors: &self.monitors,
                active_monitor: self.active,
                fullscreen: self.fullscreen,
                panel: self.show_panel,
                font_px: self.font_px,
                info,
            },
            panel: ui::Panel {
                api: self.api,
                details: self.details.as_ref(),
                error: self.details_error.as_deref(),
                commands: &self.commands,
                busy: self.busy.as_deref(),
            },
            scroll: self.scroll,
            overlay: self.overlay.as_ref(),
            notices: &self.notices,
            cursor: self.cursor,
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.window.request_redraw();
        }
    }

    fn redraw(&mut self) {
        // Out of `self` while drawing, so the controls can borrow the rest.
        let Some(mut w) = self.window.take() else {
            return;
        };
        self.draw(&mut w);
        self.window = Some(w);
    }

    fn draw(&self, w: &mut Window2) {
        let (width, height) = self.size;
        let (Some(nz_w), Some(nz_h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
            return;
        };
        if w.surface.resize(nz_w, nz_h).is_err() {
            return;
        }
        let Ok(mut buffer) = w.surface.buffer_mut() else {
            return;
        };
        let info = self.info();
        let chrome = self.chrome(&info);
        let desktop = chrome.layout.desktop;
        let view = self.view();
        match &self.picture {
            Some(p) => render::draw_picture(
                &p.pixels,
                p.width,
                p.height,
                &mut buffer,
                width,
                desktop,
                view,
            ),
            None => buffer.fill(render::BACKGROUND),
        }
        let mut canvas = render::Canvas {
            pixels: &mut buffer,
            width,
            height,
        };
        if let (Some(p), DisplayMode::Original) = (&self.picture, self.display) {
            render::draw_pan_bars(
                &mut canvas,
                desktop,
                (p.width, p.height),
                view.pan,
                (self.metrics.font / 4).max(3),
                ui::colors::MUTED,
            );
        }
        if let Some(status) = &self.status {
            // A little larger than the controls' text: it is the only thing there.
            render::draw_status(&mut canvas, desktop, self.metrics.font * 4 / 3, status);
        }
        chrome.draw(&mut canvas);
        let _ = buffer.present();
    }

    /// How the picture is shown now, the pan kept within the picture.
    fn view(&self) -> View {
        let max = match &self.picture {
            Some(p) => render::max_pan(p.width, p.height, self.layout().desktop),
            None => (0, 0),
        };
        View {
            mode: self.display,
            pan: (
                (self.pan.0.max(0.0) as u32).min(max.0),
                (self.pan.1.max(0.0) as u32).min(max.1),
            ),
        }
    }

    /// Pan speed (physical pixels a second per axis) for where the pointer
    /// is: non-zero only at original size, near an edge with more to see.
    fn pan_velocity(&self) -> (f64, f64) {
        let (Some(p), Some(cursor), DisplayMode::Original, None) =
            (&self.picture, self.cursor, self.display, &self.overlay)
        else {
            return (0.0, 0.0);
        };
        render::edge_pan(
            cursor,
            self.layout().desktop,
            (p.width, p.height),
            self.view().pan,
            f64::from(self.metrics.font) * PAN_ZONE_CHARS,
            PAN_SPEED * self.scale_factor,
        )
    }

    /// Move the picture along while the pointer is in an edge zone; the
    /// remote pointer follows what is now under the local one.
    fn pan_step(&mut self, now: Instant) {
        let (vx, vy) = self.pan_velocity();
        if vx == 0.0 && vy == 0.0 {
            self.pan_tick = None;
            return;
        }
        let Some(last) = self.pan_tick.replace(now) else {
            return;
        };
        // After a stall, a normal step rather than a leap.
        let dt = now.duration_since(last).min(PAN_TICK * 3).as_secs_f64();
        let before = self.view().pan;
        let clamp = self.view().pan;
        self.pan = (
            f64::from(clamp.0) + self.pan.0.fract() + vx * dt,
            f64::from(clamp.1) + self.pan.1.fract() + vy * dt,
        );
        self.pan = (self.pan.0.max(0.0), self.pan.1.max(0.0));
        if self.view().pan != before {
            if let Some(cursor) = self.cursor {
                self.pointer_moved(cursor);
            }
            self.request_redraw();
        }
    }

    fn send(&self, event: InputEvent) {
        if let Some(h) = &self.handle {
            h.send_input(event);
        }
    }

    fn submit(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        let time = if error {
            ERROR_NOTICE_TIME
        } else {
            NOTICE_TIME
        };
        self.notices.push(Notice {
            text: text.into(),
            error,
            until: Instant::now() + time,
        });
        if self.notices.len() > MAX_NOTICES {
            self.notices.remove(0);
        }
    }

    /// Release everything held on the remote, e.g. when the window loses
    /// focus mid-shortcut (the key-up would go to another window).
    fn release_all(&mut self) {
        for scancode in std::mem::take(&mut self.held_keys) {
            self.send(InputEvent::Key {
                scancode,
                down: false,
            });
        }
        for button in std::mem::take(&mut self.held_buttons) {
            self.send(InputEvent::MouseButton {
                button,
                down: false,
            });
        }
    }

    /// Open a menu or dialog: from now on input goes to it.
    fn open(&mut self, overlay: Overlay) {
        self.release_all();
        self.overlay = Some(overlay);
    }

    /// Forward a key to the remote by its physical position.
    fn forward_key(&mut self, key: PhysicalKey, down: bool) {
        let PhysicalKey::Code(code) = key else { return };
        let Some(scancode) = keymap::scancode(code) else {
            return;
        };
        if down {
            self.held_keys.insert(scancode);
        } else if !self.held_keys.remove(&scancode) {
            // Its key-down went elsewhere (e.g. pressed before focusing us).
            return;
        }
        self.send(InputEvent::Key { scancode, down });
    }

    /// A key while a menu or dialog is open. Nothing reaches the remote.
    fn overlay_key(&mut self, event_loop: &ActiveEventLoop, logical: &Key, text: Option<&str>) {
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        match (overlay, logical.as_ref()) {
            (_, Key::Named(NamedKey::Escape)) => {
                let cancel = match &self.overlay {
                    Some(Overlay::Prompt(_)) => Action::PromptCancel,
                    Some(Overlay::Confirm(_)) => Action::ConfirmCancel,
                    _ => {
                        self.overlay = None;
                        return;
                    }
                };
                self.act(event_loop, cancel);
            }
            (Overlay::Prompt(_), Key::Named(NamedKey::Enter)) => {
                self.act(event_loop, Action::PromptOk)
            }
            (Overlay::Confirm(_), Key::Named(NamedKey::Enter)) => {
                self.act(event_loop, Action::ConfirmOk)
            }
            (Overlay::Prompt(prompt), Key::Named(NamedKey::Backspace)) => {
                prompt.text.pop();
            }
            (Overlay::Prompt(prompt), Key::Character(c))
                if self.modifiers.control_key() && c.eq_ignore_ascii_case("v") =>
            {
                // Paste: a path copied on this machine, say.
                if let Ok(pasted) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    prompt
                        .text
                        .extend(pasted.chars().filter(|c| !c.is_control()));
                }
            }
            (Overlay::Prompt(prompt), _) if !self.modifiers.control_key() => {
                if let Some(text) = text {
                    prompt.text.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
            (Overlay::Menu(menu), Key::Character(c)) => {
                // In the monitor menu, 1-9 pick.
                let pick = digit(c)
                    .and_then(|d| menu.items.get(d - 1))
                    .map(|i| i.action.clone());
                if let Some(action @ Action::SelectMonitor(_)) = pick {
                    self.act(event_loop, action);
                }
            }
            _ => {}
        }
    }

    /// A viewer shortcut (see the module docs). Returns whether the key was
    /// consumed rather than sent to the remote.
    fn shortcut(&mut self, event_loop: &ActiveEventLoop, key: PhysicalKey) -> bool {
        let chord = ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SHIFT;
        if !self.modifiers.contains(chord) {
            return false;
        }
        let PhysicalKey::Code(code) = key else {
            return false;
        };
        match code {
            KeyCode::KeyM | KeyCode::Tab => self.act(event_loop, Action::MonitorMenu),
            KeyCode::KeyP => self.act(event_loop, Action::TogglePanel),
            KeyCode::Minus | KeyCode::NumpadSubtract => self.act(
                event_loop,
                Action::SetTextSize(self.font_px.saturating_sub(1)),
            ),
            KeyCode::Equal | KeyCode::NumpadAdd => {
                self.act(event_loop, Action::SetTextSize(self.font_px + 1))
            }
            KeyCode::KeyF => self.act(event_loop, Action::ToggleFullscreen),
            KeyCode::KeyQ => event_loop.exit(),
            KeyCode::F5 => self.act(event_loop, Action::Keyframe),
            _ => match keymap::scancode(code).and_then(|s| (0x02..=0x0A).contains(&s).then_some(s))
            {
                // Digit1..Digit9 have scancodes 0x02..0x0A.
                Some(s) => self.select_index(usize::from(s - 0x02)),
                None => return false,
            },
        }
        true
    }

    /// The toolbar button for `action`, to hang its menu from.
    fn anchor(&self, action: &Action) -> Rect {
        let info = String::new();
        self.chrome(&info)
            .toolbar_buttons()
            .into_iter()
            .find(|b| &b.action == action)
            .map(|b| b.rect)
            .unwrap_or_default()
    }

    /// Where a new path on the agent starts: the folder used last, else
    /// somewhere every user can see.
    fn remote_dir(&self) -> String {
        if let Some(dir) = &self.last_remote_dir {
            return dir.clone();
        }
        let windows = self.details.as_ref().is_none_or(AgentDetails::is_windows);
        if windows {
            r"C:\Users\Public\Desktop\"
        } else {
            "/tmp/"
        }
        .into()
    }

    fn remember_remote_dir(&mut self, remote: &str) {
        let name = remote_file_name(remote);
        if let Some(dir) = remote.strip_suffix(name) {
            if !dir.is_empty() {
                self.last_remote_dir = Some(dir.to_owned());
            }
        }
    }

    /// Do what a toolbar, panel or dialog button says.
    fn act(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::DisplayMenu
            | Action::FrameRateMenu
            | Action::MonitorMenu
            | Action::TextMenu => {
                let already = matches!(&self.overlay, Some(Overlay::Menu(m)) if m.anchor == self.anchor(&action));
                if already {
                    self.overlay = None;
                    return;
                }
                let anchor = self.anchor(&action);
                let menu = match action {
                    Action::DisplayMenu => ui::display_menu(anchor, self.display),
                    Action::FrameRateMenu => {
                        ui::frame_rate_menu(anchor, self.frame_rate, self.streamed_fps)
                    }
                    Action::TextMenu => ui::text_menu(anchor, self.font_px),
                    _ => ui::monitor_menu(anchor, &self.monitors, self.active),
                };
                self.open(Overlay::Menu(menu));
            }
            Action::SetTextSize(px) => {
                self.set_font(px);
                self.overlay = None;
                self.remember_font();
            }
            Action::SetDisplay(mode) => {
                self.display = mode;
                self.overlay = None;
                self.last_move = None;
            }
            Action::SetFrameRate(frame_rate) => {
                self.overlay = None;
                if frame_rate != self.frame_rate {
                    info!(%frame_rate, "frame rate chosen");
                    self.frame_rate = frame_rate;
                    if let Some(h) = &self.handle {
                        h.set_frame_rate(frame_rate);
                    }
                }
            }
            Action::SelectMonitor(id) => {
                if let Some(handle) = &self.handle {
                    info!(monitor = id, "selecting monitor");
                    handle.select_monitor(id);
                }
                self.overlay = None;
            }
            Action::Keyframe => {
                if let Some(h) = &self.handle {
                    h.request_keyframe();
                }
            }
            Action::ToggleFullscreen => {
                self.fullscreen = !self.fullscreen;
                if let Some(w) = &self.window {
                    w.window
                        .set_fullscreen(self.fullscreen.then_some(Fullscreen::Borderless(None)));
                }
            }
            Action::TogglePanel => {
                self.show_panel = !self.show_panel;
                self.scroll = 0;
            }
            Action::Disconnect => event_loop.exit(),
            Action::Launch(index) => {
                if let Some(command) = self.commands.get(index).cloned() {
                    self.notify(format!("Starting {}...", command.label), false);
                    self.submit(Job::Launch(command));
                }
            }
            Action::Upload => {
                self.busy = Some("Choose a file to upload...".into());
                self.submit(Job::PickUpload);
            }
            Action::Download => {
                self.open(Overlay::Prompt(ui::Prompt {
                    title: "Download which file from the agent? Its full path:".into(),
                    text: self.remote_dir(),
                    purpose: ui::PromptPurpose::Download,
                }));
            }
            Action::PromptOk => {
                let Some(Overlay::Prompt(prompt)) = self.overlay.take() else {
                    return;
                };
                let remote = prompt.text.trim().to_owned();
                if remote.is_empty() || remote_file_name(&remote).is_empty() {
                    self.notify("Give the full path of a file on the agent.", true);
                    self.overlay = Some(Overlay::Prompt(prompt));
                    return;
                }
                self.remember_remote_dir(&remote);
                match prompt.purpose {
                    ui::PromptPurpose::Upload { local } => {
                        self.busy = Some("Uploading...".into());
                        self.submit(Job::Upload {
                            local,
                            remote,
                            overwrite: false,
                        });
                    }
                    ui::PromptPurpose::Download => {
                        self.busy = Some("Choose where to save it...".into());
                        self.submit(Job::Download { remote });
                    }
                }
            }
            Action::PromptCancel => self.overlay = None,
            Action::ConfirmOk => {
                if let Some(Overlay::Confirm(confirm)) = self.overlay.take() {
                    let ui::ConfirmPurpose::Overwrite { local, remote } = confirm.purpose;
                    self.busy = Some("Uploading...".into());
                    self.submit(Job::Upload {
                        local,
                        remote,
                        overwrite: true,
                    });
                }
            }
            Action::ConfirmCancel => {
                self.overlay = None;
                self.notify("Upload cancelled.", false);
            }
        }
        self.request_redraw();
    }

    /// Pointer at window position `pos` (physical pixels).
    fn pointer_moved(&mut self, pos: (f64, f64)) {
        let Some(p) = &self.picture else {
            return;
        };
        let area = self.layout().desktop;
        let Some((px, py)) = render::unplace(pos, p.width, p.height, area, self.view()) else {
            return;
        };
        let at = (
            p.monitor,
            normalise(f64::from(px), f64::from(p.width)),
            normalise(f64::from(py), f64::from(p.height)),
        );
        if self.last_move != Some(at) {
            self.last_move = Some(at);
            self.send(InputEvent::MouseMove {
                monitor: at.0,
                x: at.1,
                y: at.2,
            });
        }
    }

    fn select_index(&mut self, index: usize) {
        if let (Some(m), Some(handle)) = (self.monitors.get(index), &self.handle) {
            info!(monitor = m.id, name = %m.name, "selecting monitor");
            handle.select_monitor(m.id);
            self.overlay = None;
        }
    }

    /// Text `px` high (clamped to the allowed range) from now on.
    fn set_font(&mut self, px: u32) {
        self.font_px = px.clamp(ui::MIN_FONT_PX, ui::MAX_FONT_PX);
        self.metrics = Metrics::new(self.font_px, self.scale_factor);
        let info = String::new();
        self.scroll = self.scroll.min(self.chrome(&info).max_scroll());
        self.hover = None;
        self.request_redraw();
    }

    /// Save the chosen text size for the next viewer, if asked to.
    fn remember_font(&self) {
        let Some(path) = &self.font_file else {
            return;
        };
        match ui::save_font_size(path, self.font_px) {
            Ok(()) => info!(size = self.font_px, path = %path.display(), "text size remembered"),
            Err(e) => warn!(path = %path.display(), "cannot remember the text size: {e}"),
        }
    }

    /// Redraw if the highlighted control changed.
    fn update_hover(&mut self) {
        let info = String::new();
        let hover = self.chrome(&info).hover_target();
        if hover != self.hover {
            self.hover = hover;
            self.request_redraw();
        }
    }

    fn scroll_panel(&mut self, lines: f64) {
        let info = String::new();
        let max = self.chrome(&info).max_scroll();
        let step = f64::from(self.metrics.line_h()) * 3.0;
        let scroll = (f64::from(self.scroll) - lines * step).clamp(0.0, f64::from(max));
        self.scroll = scroll as u32;
        self.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for App {
    /// Every way out (window close, Disconnect, the Q shortcut) passes
    /// here: tell the server, so it stops relaying (and the agent stops
    /// capturing) right away instead of after the idle timeout.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(h) = &self.handle {
            h.close();
            // Give the close a moment to leave before the process exits.
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            let now = Instant::now();
            let before = self.notices.len();
            self.notices.retain(|n| n.until > now);
            if self.notices.len() != before {
                self.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Pan while the pointer is in an edge zone (steps are timed, so
        // whatever woke the loop, the speed is right)...
        let now = Instant::now();
        match self.pan_tick {
            Some(last) if now >= last + PAN_TICK => self.pan_step(now),
            Some(_) => {}
            None if self.pan_velocity() != (0.0, 0.0) => self.pan_tick = Some(now),
            None => {}
        }
        // ...and wake up for the next step, and to take down notices.
        let next_pan = self.pan_tick.map(|t| t + PAN_TICK);
        match self.notices.iter().map(|n| n.until).chain(next_pan).min() {
            Some(until) => event_loop.set_control_flow(ControlFlow::WaitUntil(until)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(LogicalSize::new(1440.0, 860.0));
        let window = Rc::new(event_loop.create_window(attributes).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let size = window.inner_size();
        self.size = (size.width, size.height);
        self.scale_factor = window.scale_factor();
        self.metrics = Metrics::new(self.font_px, self.scale_factor);
        self.window = Some(Window2 { window, surface });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(size) => {
                self.size = (size.width, size.height);
                let info = String::new();
                self.scroll = self.scroll.min(self.chrome(&info).max_scroll());
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.set_font(self.font_px);
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Focused(false) => self.release_all(),
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                if self.overlay.is_some() {
                    if down {
                        self.overlay_key(event_loop, &event.logical_key, event.text.as_deref());
                    } else {
                        // Keys pressed before it opened still come up.
                        self.forward_key(event.physical_key, false);
                    }
                    self.request_redraw();
                    return;
                }
                // Key-ups always go through (if their key-down did), so
                // nothing sticks on the remote.
                if !(down && self.shortcut(event_loop, event.physical_key)) {
                    self.forward_key(event.physical_key, down);
                }
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let pos = (position.x, position.y);
                self.cursor = Some(pos);
                if self.overlay.is_none() {
                    self.pointer_moved(pos);
                }
                self.update_hover();
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.update_hover();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(_) => return,
                };
                if state == ElementState::Released {
                    // A release goes wherever its press went: to the remote
                    // if it started there, even if the mouse is now over
                    // the controls.
                    if let Some(i) = self.held_buttons.iter().position(|b| *b == button) {
                        self.held_buttons.remove(i);
                        self.send(InputEvent::MouseButton {
                            button,
                            down: false,
                        });
                    }
                    return;
                }
                let Some(pos) = self.cursor else { return };
                let info = String::new();
                let hit = self.chrome(&info).hit(pos);
                match hit {
                    Hit::Desktop => {
                        self.held_buttons.push(button);
                        self.send(InputEvent::MouseButton { button, down: true });
                    }
                    Hit::Action(action) if button == MouseButton::Left => {
                        self.act(event_loop, action)
                    }
                    Hit::Dismiss => {
                        self.overlay = None;
                        self.request_redraw();
                    }
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let over_panel = self
                    .cursor
                    .is_some_and(|c| self.layout().panel.is_some_and(|p| p.contains(c)));
                if over_panel {
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                        MouseScrollDelta::PixelDelta(p) => p.y / 40.0,
                    };
                    self.scroll_panel(lines);
                    return;
                }
                let over_desktop = self
                    .cursor
                    .is_some_and(|c| self.layout().desktop.contains(c));
                if self.overlay.is_none() && over_desktop {
                    let (dx, dy) = wheel(delta);
                    if dx != 0 || dy != 0 {
                        self.send(InputEvent::Wheel { dx, dy });
                    }
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Status(status) => self.status = Some(status),
            UserEvent::Connected(welcome, handle) => {
                self.agent_id = Some(welcome.agent_id);
                self.monitors = welcome.monitors;
                self.active = welcome.active_monitor;
                self.handle = Some(handle);
                self.status = Some("Waiting for video...".into());
            }
            UserEvent::Picture => {
                let picture = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(p) = picture {
                    self.active = Some(p.monitor);
                    self.picture = Some(p);
                    self.status = None;
                    self.frames += 1;
                    let elapsed = self.fps_since.elapsed();
                    if elapsed >= Duration::from_secs(1) {
                        self.fps = f64::from(self.frames) / elapsed.as_secs_f64();
                        self.frames = 0;
                        self.fps_since = Instant::now();
                    }
                }
            }
            UserEvent::Monitors(monitors) => self.monitors = monitors,
            UserEvent::StreamMonitor(monitor) => self.active = Some(monitor),
            UserEvent::StreamStatus(status) => {
                info!(
                    frame_rate = %status.frame_rate,
                    fps = status.fps,
                    bitrate = status.bitrate,
                    "stream status"
                );
                self.streamed_fps = Some(status.fps);
            }
            // A failed attempt leaves the session where it was: relayed.
            UserEvent::Path(protocol::e2e::Path::DirectFailed) => {}
            UserEvent::Path(path) => self.path = path,
            UserEvent::Closed(reason) => self.status = Some(format!("Disconnected: {reason}")),
            UserEvent::Details(Ok(details)) => {
                self.details = Some(*details);
                self.details_error = None;
            }
            UserEvent::Details(Err(error)) => self.details_error = Some(error),
            UserEvent::Notice { text, error } => self.notify(text, error),
            UserEvent::Busy(busy) => self.busy = busy,
            UserEvent::UploadPicked(local) => {
                self.busy = None;
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let text = format!("{}{name}", self.remote_dir());
                self.open(Overlay::Prompt(ui::Prompt {
                    title: format!("Upload {name} to which path on the agent?"),
                    text,
                    purpose: ui::PromptPurpose::Upload { local },
                }));
            }
            UserEvent::UploadExists { local, remote } => {
                self.busy = None;
                self.open(Overlay::Confirm(ui::Confirm {
                    text: format!("{remote} already exists on the agent. Replace it?"),
                    ok: "Replace".into(),
                    purpose: ui::ConfirmPurpose::Overwrite { local, remote },
                }));
            }
        }
        let title = self.title();
        if title != self.window_title {
            if let Some(w) = &self.window {
                w.window.set_title(&title);
            }
            self.window_title = title;
        }
        self.request_redraw();
    }
}

fn digit(c: &str) -> Option<usize> {
    c.chars()
        .next()
        .and_then(|c| c.to_digit(10))
        .filter(|d| *d >= 1)
        .map(|d| d as usize)
}

/// winit scroll to wheel units. winit's positive values move the content
/// right/down, i.e. scroll left/up; Windows' positive wheel is up, and
/// positive horizontal wheel is right.
fn wheel(delta: MouseScrollDelta) -> (i16, i16) {
    let clamp = |v: f64| v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (
            clamp(-f64::from(x) * f64::from(WHEEL_NOTCH)),
            clamp(f64::from(y) * f64::from(WHEEL_NOTCH)),
        ),
        // Touchpads: about 40 pixels per notch.
        MouseScrollDelta::PixelDelta(p) => (
            clamp(-p.x * f64::from(WHEEL_NOTCH) / 40.0),
            clamp(p.y * f64::from(WHEEL_NOTCH) / 40.0),
        ),
    }
}
