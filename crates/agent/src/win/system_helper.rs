//! The system helper: runs as SYSTEM in the console session, and does
//! there what the session helper, running as the user, cannot.
//!
//! - It injects the technician's input. The session helper runs as the
//!   user, and Windows drops input from a lower integrity level aimed at a
//!   higher one (UIPI), so it cannot type into or click elevated windows.
//!   SYSTEM outranks every window in the session.
//! - It follows the input desktop (see `super::desktop`). The logon
//!   screen, the lock screen and UAC prompts are on the secure desktop,
//!   which only SYSTEM may attach to. While that desktop is showing, or
//!   nobody is logged on, this process also captures the screen; the
//!   service is told which desktop is showing and asks whichever helper
//!   can see it (see `super::bridge`).
//!
//! It runs whenever there is a console session, logged on or not (see
//! `process::spawn_system_in_session`). It does nothing else: no windows,
//! no clipboard, no prompts, so there is little for the user to attack in
//! a SYSTEM process on their desktop.
//!
//! It only obeys the service that started it. The pipe's ACL lets
//! interactive users open it, so before trusting the server end the helper
//! checks that it belongs to that service's process. Otherwise a user
//! could stand up their own pipe instance and have SYSTEM inject input
//! into elevated windows on their behalf.

use std::cell::RefCell;
use std::os::windows::io::AsRawHandle;
use std::time::Duration;

use anyhow::{bail, Context};
use protocol::ipc::IpcMessage;
use protocol::{read_frame, write_frame};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use super::desktop::{self, Follower};
use super::helper::{connect, current_session_id};
use super::input::Injector;
use super::stream::{self, Desktops, WorkerCommand};

/// How often to check which desktop is receiving input.
const DESKTOP_POLL: Duration = Duration::from_millis(250);

/// Frames and monitor lists queued for the service (as the session
/// helper's: if the service is behind, the capture worker drops frames).
const OUT_QUEUE: usize = 8;

pub fn run(service_pid: u32) -> anyhow::Result<()> {
    common::logging::init_file(&crate::paths::system_helper_log());
    // Same coordinate space as the session helper (see `helper::run`).
    // SAFETY: process-wide setting, no pointers.
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        warn!("setting DPI awareness: {e}");
    }
    let session_id = current_session_id();
    info!(
        pid = std::process::id(),
        ?session_id,
        service_pid,
        "system helper starting"
    );
    // One thread: input is injected on the thread that follows the desktop.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve(service_pid, session_id.unwrap_or(0)));
    info!("system helper exiting");
    result
}

async fn serve(service_pid: u32, session_id: u32) -> anyhow::Result<()> {
    let pipe = connect().await.context("connecting to the service")?;
    let mut server_pid = 0;
    // SAFETY: valid pipe handle; out-parameter.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server_pid) }
        .context("identifying the pipe server")?;
    if server_pid != service_pid {
        bail!("pipe server is pid {server_pid}, not the service ({service_pid}); refusing it");
    }
    let (mut reader, mut writer) = tokio::io::split(pipe);
    write_frame(
        &mut writer,
        &IpcMessage::HelperHello {
            pid: std::process::id(),
            session_id,
            version: crate::core::version().to_owned(),
        },
    )
    .await?;
    info!("connected to service");

    // Which desktop is showing, to the service; ahead of video.
    let (ctl_tx, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel::<IpcMessage>();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<IpcMessage>(OUT_QUEUE);
    let worker = stream::spawn(out_tx, Desktops::Input);
    // This thread's desktop: shared by the poll and the injection below,
    // which take turns on the one thread.
    let follower = RefCell::new(Follower::default());

    let write = async {
        loop {
            let message = tokio::select! {
                biased;
                Some(m) = ctl_rx.recv() => m,
                Some(m) = out_rx.recv() => m,
                else => break,
            };
            write_frame(&mut writer, &message).await?;
        }
        anyhow::Ok(())
    };
    let watch = async {
        let mut reported = None;
        loop {
            match follower.borrow_mut().follow() {
                Ok(name) => {
                    let secure = desktop::is_secure(name);
                    if reported != Some(secure) {
                        info!(desktop = name, secure, "input desktop");
                        reported = Some(secure);
                        if ctl_tx.send(IpcMessage::Desktop { secure }).is_err() {
                            break;
                        }
                    }
                }
                // Mid-switch; the next poll sees where it ended up.
                Err(e) => debug!("input desktop unavailable: {e}"),
            }
            tokio::time::sleep(DESKTOP_POLL).await;
        }
        anyhow::Ok(())
    };
    let read = async {
        let mut injector = Injector::default();
        while let Some(message) = read_frame::<_, IpcMessage>(&mut reader).await? {
            let command = match message {
                IpcMessage::Input(event) => {
                    if !injector.inject(event) {
                        // The desktop changed since the last poll.
                        let moved = follower.borrow_mut().follow().is_ok();
                        if !(moved && injector.inject(event)) {
                            warn!(?event, "input was blocked");
                        }
                    }
                    continue;
                }
                IpcMessage::ListMonitors => WorkerCommand::ListMonitors,
                IpcMessage::StartCapture { monitor } => WorkerCommand::Start { monitor },
                IpcMessage::StopCapture => WorkerCommand::Stop,
                IpcMessage::ForceKeyframe => WorkerCommand::ForceKeyframe,
                IpcMessage::SetBitrate { bps } => WorkerCommand::SetBitrate(bps),
                IpcMessage::SetFrameRate { fps } => WorkerCommand::SetFrameRate(fps),
                other => {
                    debug!(?other, "ignoring message from service");
                    continue;
                }
            };
            if worker.send(command).is_err() {
                bail!("the capture worker stopped");
            }
        }
        Ok(())
    };
    tokio::select! {
        r = write => r,
        r = watch => r,
        r = read => r,
    }
}
