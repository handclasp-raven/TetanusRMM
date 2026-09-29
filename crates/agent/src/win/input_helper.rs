//! The input helper: injects the technician's input, as SYSTEM, on the
//! logged-on user's desktop.
//!
//! The session helper runs as the user, and Windows drops input from a
//! lower integrity level aimed at a higher one (UIPI), so it cannot type
//! into or click elevated windows. This process runs as SYSTEM in the
//! user's session (see `process::spawn_system_in_session`), which outranks
//! every window there. It does nothing else: no windows, no clipboard, no
//! capture, so there is little for the user to attack in a SYSTEM process
//! on their desktop.
//!
//! It only accepts input from the service that started it. The pipe's
//! ACL lets interactive users open it, so before trusting the server end
//! the helper checks that it belongs to that service's process. Otherwise
//! a user could stand up their own pipe instance and have SYSTEM inject
//! input into elevated windows on their behalf.

use std::os::windows::io::AsRawHandle;

use anyhow::{bail, Context};
use protocol::ipc::IpcMessage;
use protocol::{read_frame, write_frame};
use tracing::{debug, info};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use super::helper::{connect, current_session_id};
use super::input::Injector;

pub fn run(service_pid: u32) -> anyhow::Result<()> {
    common::logging::init_file(&crate::paths::input_helper_log());
    // Same coordinate space as the session helper (see `helper::run`).
    // SAFETY: process-wide setting, no pointers.
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        tracing::warn!("setting DPI awareness: {e}");
    }
    let session_id = current_session_id();
    info!(
        pid = std::process::id(),
        ?session_id,
        service_pid,
        "input helper starting"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve(service_pid, session_id.unwrap_or(0)));
    info!("input helper exiting");
    result
}

async fn serve(service_pid: u32, session_id: u32) -> anyhow::Result<()> {
    let mut pipe = connect().await.context("connecting to the service")?;
    let mut server_pid = 0;
    // SAFETY: valid pipe handle; out-parameter.
    unsafe { GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut server_pid) }
        .context("identifying the pipe server")?;
    if server_pid != service_pid {
        bail!("pipe server is pid {server_pid}, not the service ({service_pid}); refusing it");
    }
    write_frame(
        &mut pipe,
        &IpcMessage::HelperHello {
            pid: std::process::id(),
            session_id,
            version: crate::core::version().to_owned(),
        },
    )
    .await?;
    info!("connected to service");

    let mut injector = Injector::default();
    while let Some(message) = read_frame::<_, IpcMessage>(&mut pipe).await? {
        match message {
            IpcMessage::Input(event) => injector.inject(event),
            other => debug!(?other, "ignoring message from service"),
        }
    }
    Ok(())
}
