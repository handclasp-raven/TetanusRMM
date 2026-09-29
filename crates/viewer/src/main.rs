//! Remote desktop viewer. Connects to the server (not the agent) with a
//! short-lived viewer-session token and shows the agent's screen.
//!
//! Keys: Tab or M toggles the monitor picker, 1-9 picks a monitor, F5 asks
//! for a fresh keyframe, Esc closes the picker (or the viewer).

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use clap::Parser;
use protocol::media::MonitorInfo;
use tracing::{info, warn};
use viewer::client::{self, ViewerEvent, ViewerHandle, ViewerOptions, Welcome};
use viewer::decode::{Picture, VideoDecoder};
use viewer::render;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

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
    /// Viewer-session token (from POST /api/agents/{id}/viewer-sessions).
    #[arg(long, env = "RMM_VIEWER_TOKEN", hide_env_values = true)]
    token: String,
    /// Switch to this monitor id after connecting.
    #[arg(long)]
    monitor: Option<u32>,
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
    let options = ViewerOptions {
        server: cli.server,
        server_name: cli.server_name.clone(),
        ca_pem: std::fs::read_to_string(&cli.ca)
            .with_context(|| format!("reading {}", cli.ca.display()))?,
        token: cli.token.clone(),
        bind: None,
    };
    match &cli.snapshot {
        Some(path) => snapshot(options, cli.monitor, cli.frames, path),
        None => windowed(options, cli.monitor),
    }
}

// ---------------------------------------------------------------------------
// Headless snapshot mode.

fn snapshot(
    options: ViewerOptions,
    monitor: Option<u32>,
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

enum UserEvent {
    Connected(Welcome, ViewerHandle),
    /// A new picture is in the shared slot.
    Picture,
    Monitors(Vec<MonitorInfo>),
    StreamMonitor(u32),
    Closed(String),
}

/// The newest decoded picture. The decoder overwrites it; the window takes
/// it. A slow window skips pictures instead of queueing them.
type Slot = Arc<Mutex<Option<Picture>>>;

fn windowed(options: ViewerOptions, monitor: Option<u32>) -> anyhow::Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let slot: Slot = Arc::new(Mutex::new(None));
    std::thread::spawn({
        let slot = slot.clone();
        move || network(options, monitor, proxy, slot)
    });
    let mut app = App {
        slot,
        window: None,
        handle: None,
        agent_id: None,
        monitors: Vec::new(),
        active: None,
        picture: None,
        show_picker: false,
        status: Some("Connecting...".into()),
        frames: 0,
        fps: 0.0,
        fps_since: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

/// Network + decode, off the UI thread.
fn network(
    options: ViewerOptions,
    monitor: Option<u32>,
    proxy: EventLoopProxy<UserEvent>,
    slot: Slot,
) {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        let (welcome, handle, mut events) = match client::connect(&options).await {
            Ok(connected) => connected,
            Err(e) => {
                let _ = proxy.send_event(UserEvent::Closed(e.to_string()));
                return;
            }
        };
        if let Some(m) = monitor {
            handle.select_monitor(m);
        }
        let _ = proxy.send_event(UserEvent::Connected(welcome, handle.clone()));

        // Decoding is CPU work: its own thread, fed through a channel.
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::channel(64);
        std::thread::spawn({
            let proxy = proxy.clone();
            let handle = handle.clone();
            move || {
                let mut decoder = VideoDecoder::new().expect("OpenH264 decoder");
                while let Some(frame) = frames_rx.blocking_recv() {
                    match decoder.decode(&frame) {
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
        });

        while let Some(event) = events.recv().await {
            let user_event = match event {
                ViewerEvent::Frame(frame) => {
                    if frames_tx.send(frame).await.is_err() {
                        break;
                    }
                    continue;
                }
                ViewerEvent::Monitors(m) => UserEvent::Monitors(m),
                ViewerEvent::StreamMonitor(m) => UserEvent::StreamMonitor(m),
                ViewerEvent::Closed(reason) => UserEvent::Closed(reason),
            };
            if proxy.send_event(user_event).is_err() {
                break;
            }
        }
    });
}

struct Window2 {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
}

struct App {
    slot: Slot,
    window: Option<Window2>,
    handle: Option<ViewerHandle>,
    agent_id: Option<String>,
    monitors: Vec<MonitorInfo>,
    active: Option<u32>,
    picture: Option<Picture>,
    show_picker: bool,
    status: Option<String>,
    frames: u32,
    fps: f64,
    fps_since: Instant,
}

impl App {
    fn title(&self) -> String {
        let agent = self.agent_id.as_deref().unwrap_or("connecting");
        let monitor = self
            .active
            .and_then(|a| self.monitors.iter().find(|m| m.id == a))
            .map(|m| format!("{} {}x{}", m.name, m.width, m.height))
            .unwrap_or_else(|| "-".into());
        format!(
            "RMM Viewer - {agent} - {monitor} - {:.0} fps - Tab: monitors",
            self.fps
        )
    }

    fn redraw(&mut self) {
        let Some(w) = &mut self.window else { return };
        let size = w.window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if w.surface.resize(width, height).is_err() {
            return;
        }
        let Ok(mut buffer) = w.surface.buffer_mut() else {
            return;
        };
        let (dw, dh) = (size.width, size.height);
        match &self.picture {
            Some(p) => render::draw_picture(&p.pixels, p.width, p.height, &mut buffer, dw, dh),
            None => buffer.fill(render::BACKGROUND),
        }
        let mut canvas = render::Canvas {
            pixels: &mut buffer,
            width: dw,
            height: dh,
        };
        if let Some(status) = &self.status {
            render::draw_status(&mut canvas, status);
        }
        if self.show_picker {
            render::draw_picker(&mut canvas, &self.monitors, self.active);
        }
        let _ = buffer.present();
    }

    fn select_index(&mut self, index: usize) {
        if let (Some(m), Some(handle)) = (self.monitors.get(index), &self.handle) {
            info!(monitor = m.id, name = %m.name, "selecting monitor");
            handle.select_monitor(m.id);
            self.show_picker = false;
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    /// Every way out (window close, Esc) passes here: tell the server, so
    /// it stops relaying (and the agent stops capturing) right away instead
    /// of after the idle timeout.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(h) = &self.handle {
            h.close();
            // Give the close a moment to leave before the process exits.
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Rc::new(event_loop.create_window(attributes).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        self.window = Some(Window2 { window, surface });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => {
                if let Some(w) = &self.window {
                    w.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Tab) => self.show_picker = !self.show_picker,
                    Key::Character("m") | Key::Character("M") => {
                        self.show_picker = !self.show_picker
                    }
                    Key::Named(NamedKey::Escape) if self.show_picker => self.show_picker = false,
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::F5) => {
                        if let Some(h) = &self.handle {
                            h.request_keyframe();
                        }
                    }
                    Key::Character(c) => {
                        if let Some(d) = c
                            .chars()
                            .next()
                            .and_then(|c| c.to_digit(10))
                            .filter(|d| *d >= 1)
                        {
                            self.select_index(d as usize - 1);
                        }
                    }
                    _ => {}
                }
                if let Some(w) = &self.window {
                    w.window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
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
            UserEvent::Closed(reason) => self.status = Some(format!("Disconnected: {reason}")),
        }
        if let Some(w) = &self.window {
            w.window.set_title(&self.title());
            w.window.request_redraw();
        }
    }
}
