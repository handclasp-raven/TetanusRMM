//! The agent as a Windows service: install/uninstall/start/stop, and the
//! service entry point the Service Control Manager (SCM) calls.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use tokio::sync::{watch, Notify};
use tracing::{error, info, warn};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use super::bridge::Bridge;
use super::{acl, pipe, process};
use crate::core::{self, CoreOptions};
use crate::credstore::{Credential, CredentialStore};
use crate::enroll::{self, EnrollRequest};
use crate::interactive::desktop_channel;
use crate::media::source::media_channel;
use crate::session::{Action, Backoff, Supervisor};
use crate::telemetry::SystemTelemetry;
use crate::update::VerifiedUpdate;
use crate::updater::{self, UpdatePaths};
use crate::{paths, DEFAULT_HEARTBEAT_INTERVAL};

pub const SERVICE_NAME: &str = "RmmAgent";
const DISPLAY_NAME: &str = "RMM Agent";
const DESCRIPTION: &str = "Remote monitoring and management agent.";

/// Service-specific exit code after installing an update. It is a non-zero
/// exit, so the SCM's failure actions restart the service, now running the
/// new binary.
pub const EXIT_UPDATE_INSTALLED: u32 = 1;
/// Service-specific exit code for an unexpected error (also restarted).
pub const EXIT_ERROR: u32 = 2;

/// Retry interval while waiting to be able to enroll.
const ENROLL_RETRY: Duration = Duration::from_secs(30);
const UPDATE_INTERVAL: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------
// Management commands (run by an administrator from a console).

pub struct InstallOptions {
    pub server: SocketAddr,
    pub server_name: String,
    pub server_ca: Option<PathBuf>,
    pub token: Option<String>,
    pub start: bool,
}

/// Install the service: copy this binary to `%ProgramFiles%\RMM`, lock down
/// the state directory, leave an enrollment request for the service, and
/// register an auto-start LocalSystem service that restarts on failure.
pub fn install(opts: InstallOptions) -> anyhow::Result<()> {
    let state_dir = paths::default_state_dir();
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    acl::restrict_to_system_and_admins(&state_dir)
        .with_context(|| format!("restricting access to {}", state_dir.display()))?;

    match (&opts.token, &opts.server_ca) {
        (Some(token), Some(ca)) => {
            EnrollRequest {
                server_addr: opts.server,
                server_name: opts.server_name.clone(),
                server_ca_pem: std::fs::read_to_string(ca)
                    .with_context(|| format!("reading {}", ca.display()))?,
                token: token.clone(),
            }
            .save(&state_dir)
            .context("saving enrollment request")?;
        }
        (Some(_), None) => bail!("--server-ca is required with --token"),
        (None, _) if !CredentialStore::new(&state_dir).exists() => {
            bail!("not enrolled yet: pass --token and --server-ca from a download link")
        }
        (None, _) => {}
    }

    let install_dir = paths::install_dir();
    std::fs::create_dir_all(&install_dir)?;
    let exe = install_dir.join(paths::EXE_NAME);
    let current = std::env::current_exe()?;
    if !same_file(&current, &exe) {
        std::fs::copy(&current, &exe)
            .with_context(|| format!("copying agent to {}", exe.display()))?;
    }

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
            .context("opening the service manager (run as Administrator)")?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec!["service".into(), "run".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("creating the service (already installed? run `service uninstall` first)")?;
    service.set_description(DESCRIPTION)?;
    // Restart on crash, and on our own non-zero exits (after an update).
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            restart_after(Duration::from_secs(5)),
            restart_after(Duration::from_secs(5)),
            restart_after(Duration::from_secs(30)),
        ]),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    info!(exe = %exe.display(), state_dir = %state_dir.display(), "service installed");

    if opts.start {
        service.start::<&str>(&[]).context("starting the service")?;
        info!("service started");
    }
    Ok(())
}

fn restart_after(delay: Duration) -> ServiceAction {
    ServiceAction {
        action_type: ServiceActionType::Restart,
        delay,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Stop (waiting for it) and delete the service. Files are left in place.
pub fn uninstall() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .context("opening the service (is it installed?)")?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
        wait_for_state(|| service.query_status(), ServiceState::Stopped)?;
    }
    service.delete()?;
    info!(
        binary = %paths::install_dir().display(),
        state = %paths::default_state_dir().display(),
        "service removed; files left in place"
    );
    Ok(())
}

pub fn start() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.start::<&str>(&[])?;
    wait_for_state(|| service.query_status(), ServiceState::Running)
}

pub fn stop() -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    service.stop()?;
    wait_for_state(|| service.query_status(), ServiceState::Stopped)
}

fn wait_for_state(
    query: impl Fn() -> windows_service::Result<ServiceStatus>,
    want: ServiceState,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = query()?.current_state;
        if state == want {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("service is {state:?}, expected {want:?}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// Service entry point (run by the SCM as LocalSystem).

define_windows_service!(ffi_service_main, service_main);

/// Hand this process to the SCM. Only returns once the service has stopped.
/// Fails if the process was not started by the SCM.
pub fn run_dispatcher() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_args: Vec<OsString>) {
    let state_dir = paths::default_state_dir();
    common::logging::init_file(&state_dir.join("agent.log"));
    if let Err(e) = run_service(&state_dir) {
        error!("service failed: {e:#}");
    }
}

enum Exit {
    Stopped,
    UpdateInstalled,
}

fn run_service(state_dir: &Path) -> anyhow::Result<()> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let session_changed = Arc::new(Notify::new());
    let notify = session_changed.clone();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
            let _ = shutdown_tx.send(true);
            ServiceControlHandlerResult::NoError
        }
        // Logon, logoff, lock, user switch: re-check the console session now
        // rather than at the next poll.
        ServiceControl::SessionChange(_) => {
            notify.notify_one();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;
    let status = |state, exit_code| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP
                | ServiceControlAccept::SHUTDOWN
                | ServiceControlAccept::SESSION_CHANGE
        } else {
            ServiceControlAccept::empty()
        },
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    };
    status_handle.set_service_status(status(ServiceState::Running, ServiceExitCode::Win32(0)))?;
    info!(version = core::version(), "service running");

    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(service_body(state_dir, shutdown_rx, session_changed));
    let exit_code = match &result {
        Ok(Exit::Stopped) => {
            info!("service stopped");
            ServiceExitCode::Win32(0)
        }
        Ok(Exit::UpdateInstalled) => {
            info!("exiting so the service manager restarts the updated binary");
            ServiceExitCode::ServiceSpecific(EXIT_UPDATE_INSTALLED)
        }
        Err(e) => {
            error!("service error: {e:#}");
            ServiceExitCode::ServiceSpecific(EXIT_ERROR)
        }
    };
    runtime.shutdown_timeout(Duration::from_secs(2));
    status_handle.set_service_status(status(ServiceState::Stopped, exit_code))?;
    Ok(())
}

async fn service_body(
    state_dir: &Path,
    mut shutdown: watch::Receiver<bool>,
    session_changed: Arc<Notify>,
) -> anyhow::Result<Exit> {
    let paths = UpdatePaths::for_current_exe()?;
    updater::cleanup(&paths);

    let (status_tx, status_rx) = watch::channel(core::initial_status(None));
    let helper_pid = Arc::new(AtomicU32::new(0));
    // Screen streaming: the core talks to the helper through the bridge.
    let (media_link, media_source) = media_channel();
    // Consent, input and clipboard: likewise, through the helper. A user is
    // "present" (to be asked or told) when someone is logged on at the console.
    let (desktop_link, desktop_end) = desktop_channel(|| process::console_user_session().is_some());
    let bridge = Bridge::start(media_source, desktop_end);
    tokio::spawn({
        let helper_pid = helper_pid.clone();
        async move {
            if let Err(e) = pipe::serve(status_rx, helper_pid, Some(bridge)).await {
                error!("helper pipe server stopped: {e}");
            }
        }
    });
    let (stop_helper_tx, stop_helper_rx) = watch::channel(false);
    let supervisor = tokio::spawn(supervise_helper(
        paths.current.clone(),
        helper_pid,
        session_changed,
        stop_helper_rx,
    ));

    let agent = async {
        let credential = obtain_credential(state_dir).await?;
        status_tx.send_modify(|s| s.agent_id = Some(credential.agent_id.clone()));
        let options = CoreOptions {
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            update_interval: UPDATE_INTERVAL,
            telemetry: Some(Arc::new(SystemTelemetry::new())),
            media: Some(Arc::new(media_link)),
            desktop: Some(Arc::new(desktop_link)),
        };
        core::run(&credential, options, status_tx).await
    };

    let result = tokio::select! {
        _ = shutdown.wait_for(|stop| *stop) => Ok(Exit::Stopped),
        update = agent => install_update(&paths, update?).map(|()| Exit::UpdateInstalled),
    };
    let _ = stop_helper_tx.send(true);
    let _ = supervisor.await;
    result
}

fn install_update(paths: &UpdatePaths, update: VerifiedUpdate) -> anyhow::Result<()> {
    info!(version = %update.version, "installing update");
    updater::stage(paths, &update.binary).context("staging update")?;
    updater::swap(paths).context("swapping in update")
}

/// Load the credential, or enroll using the request left by `service install`.
/// Keeps retrying (network down, server unreachable) until it succeeds.
async fn obtain_credential(state_dir: &Path) -> anyhow::Result<Credential> {
    let store = CredentialStore::new(state_dir);
    loop {
        if store.exists() {
            return store.load().context("loading credential");
        }
        match EnrollRequest::load(state_dir)? {
            None => warn!(
                "not enrolled and no enrollment request; reinstall with --token and --server-ca"
            ),
            Some(request) => match enroll::enroll(&request.options()).await {
                Ok(credential) => {
                    store.save(&credential)?;
                    EnrollRequest::remove(state_dir)?;
                    info!(agent_id = %credential.agent_id, "enrolled; credential protected with DPAPI as SYSTEM");
                    return Ok(credential);
                }
                Err(e) => warn!("enrollment failed, will retry: {e}"),
            },
        }
        tokio::time::sleep(ENROLL_RETRY).await;
    }
}

/// Keep a helper running in the console user's session (see
/// [`crate::session`]). Terminates the helper when told to stop.
async fn supervise_helper(
    exe: PathBuf,
    helper_pid: Arc<AtomicU32>,
    session_changed: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let mut supervisor = Supervisor::new(Backoff::default());
    let mut helper: Option<process::HelperProcess> = None;
    loop {
        let now = Instant::now();
        let running = helper.as_ref().is_some_and(|h| h.is_running());
        if let Some(h) = helper.as_ref().filter(|_| !running) {
            info!(pid = h.pid, exit_code = ?h.exit_code(), "helper exited");
            helper = None;
            helper_pid.store(0, Ordering::SeqCst);
        }
        let console = process::console_user_session();
        match supervisor.poll(now, console, running) {
            Action::Idle => {}
            Action::Spawn { session_id } => {
                match process::spawn_in_session(session_id, &exe, "helper") {
                    Ok(h) => {
                        // Publish the pid before the helper can connect.
                        helper_pid.store(h.pid, Ordering::SeqCst);
                        h.resume();
                        info!(pid = h.pid, session_id, "helper started");
                        helper = Some(h);
                        supervisor.spawned(now, session_id, true);
                    }
                    Err(e) => {
                        supervisor.spawned(now, session_id, false);
                        warn!(
                            session_id,
                            failures = supervisor.failures(),
                            "could not start helper: {e}"
                        );
                    }
                }
            }
            Action::Terminate => {
                if let Some(h) = helper.take() {
                    info!(
                        pid = h.pid,
                        session_id = h.session_id,
                        "console user changed; stopping helper"
                    );
                    h.terminate();
                    helper_pid.store(0, Ordering::SeqCst);
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(supervisor.next_wake(now)) => {}
            () = session_changed.notified() => {}
            _ = stop.wait_for(|stop| *stop) => {
                if let Some(h) = helper.take() {
                    h.terminate();
                }
                return;
            }
        }
    }
}
