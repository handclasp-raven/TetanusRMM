//! Service side of the helper pipe.
//!
//! Access control, in layers:
//! 1. The pipe's DACL lets only SYSTEM, Administrators and interactively
//!    logged-on users (IU) connect, and remote clients are rejected.
//! 2. The service creates the first instance with `first_pipe_instance`, so
//!    if another process already owns the name, creation fails instead of
//!    the service unknowingly sharing it.
//! 3. Each client's process id must be a helper the service itself just
//!    spawned (the session helper or the input helper); anything else is
//!    disconnected.
//! 4. In the other direction, the input helper (SYSTEM) checks that the
//!    server end belongs to the service before it injects anything, since
//!    the DACL lets interactive users create instances too.

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use protocol::ipc::{AgentStatus, IpcMessage, PIPE_NAME};
use protocol::{read_frame, write_frame};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;

use super::acl::SecurityDescriptor;
use super::bridge::Bridge;

/// SYSTEM and Administrators: full. Interactive users: read/write.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

fn client_pid(pipe: &NamedPipeServer) -> Option<u32> {
    let mut pid = 0;
    // SAFETY: the raw handle is valid for the lifetime of `pipe`.
    unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid).ok()? };
    Some(pid)
}

/// Pids of the helpers the supervisor last spawned (0 if none), published
/// before each can connect.
#[derive(Clone, Default)]
pub struct HelperPids {
    pub helper: Arc<AtomicU32>,
    pub input: Arc<AtomicU32>,
}

/// Accept helper connections forever.
pub async fn serve(
    status: watch::Receiver<AgentStatus>,
    pids: HelperPids,
    bridge: Option<Arc<Bridge>>,
) -> std::io::Result<()> {
    serve_on(PIPE_NAME, status, pids, bridge).await
}

/// [`serve`] on a given pipe name (tests use a unique one).
pub async fn serve_on(
    pipe_name: &str,
    status: watch::Receiver<AgentStatus>,
    pids: HelperPids,
    bridge: Option<Arc<Bridge>>,
) -> std::io::Result<()> {
    let sd = SecurityDescriptor::from_sddl(PIPE_SDDL).map_err(std::io::Error::other)?;
    let mut first = true;
    loop {
        // Scoped so the raw pointer in `attrs` is not held across an await.
        let server = {
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.as_ptr(),
                bInheritHandle: false.into(),
            };
            // SAFETY: `attrs` and the descriptor it points to outlive the call.
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(
                        pipe_name,
                        (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
                    )?
            }
        };
        first = false;
        server.connect().await?;

        let helper = pids.helper.load(Ordering::SeqCst);
        let input = pids.input.load(Ordering::SeqCst);
        match client_pid(&server) {
            Some(pid) if pid == helper && pid != 0 => {
                info!(pid, "helper connected");
                tokio::spawn(serve_helper(server, status.clone(), bridge.clone()));
            }
            Some(pid) if pid == input && pid != 0 => {
                info!(pid, "input helper connected");
                tokio::spawn(serve_input_helper(server, bridge.clone()));
            }
            other => {
                warn!(
                    client = ?other,
                    helper,
                    input,
                    "rejected pipe client that is not our helper"
                );
            }
        }
    }
}

async fn serve_helper(
    pipe: NamedPipeServer,
    mut status: watch::Receiver<AgentStatus>,
    bridge: Option<Arc<Bridge>>,
) {
    let (mut reader, mut writer) = tokio::io::split(pipe);
    match read_frame::<_, IpcMessage>(&mut reader).await {
        Ok(Some(IpcMessage::HelperHello {
            pid,
            session_id,
            version,
        })) => info!(pid, session_id, %version, "helper hello"),
        other => {
            warn!(?other, "helper did not say hello");
            return;
        }
    }

    // Commands for the helper from the media bridge.
    let (to_helper, mut outbox) = mpsc::unbounded_channel::<IpcMessage>();
    if let Some(bridge) = &bridge {
        bridge.attach(to_helper.clone());
    }

    // The first status goes out immediately.
    status.mark_changed();
    let write = async {
        loop {
            let message = tokio::select! {
                changed = status.changed() => {
                    if changed.is_err() { break; }
                    IpcMessage::Status(status.borrow_and_update().clone())
                }
                Some(message) = outbox.recv() => message,
            };
            if write_frame(&mut writer, &message).await.is_err() {
                break;
            }
        }
    };

    let read = async {
        loop {
            match read_frame::<_, IpcMessage>(&mut reader).await {
                Ok(Some(message)) => {
                    if let Some(bridge) = &bridge {
                        bridge.from_helper(message).await;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("helper pipe: {e}");
                    break;
                }
            }
        }
    };

    tokio::select! {
        () = write => {}
        () = read => {}
    }
    if let Some(bridge) = &bridge {
        bridge.detach(&to_helper);
    }
    drop(to_helper);
    info!("helper disconnected");
}

/// The input helper only receives input; it sends nothing after its hello.
async fn serve_input_helper(pipe: NamedPipeServer, bridge: Option<Arc<Bridge>>) {
    let (mut reader, mut writer) = tokio::io::split(pipe);
    match read_frame::<_, IpcMessage>(&mut reader).await {
        Ok(Some(IpcMessage::HelperHello {
            pid,
            session_id,
            version,
        })) => {
            info!(pid, session_id, %version, "input helper hello")
        }
        other => {
            warn!(?other, "input helper did not say hello");
            return;
        }
    }
    let (to_injector, mut outbox) = mpsc::unbounded_channel::<IpcMessage>();
    if let Some(bridge) = &bridge {
        bridge.attach_injector(to_injector.clone());
    }
    let write = async {
        while let Some(message) = outbox.recv().await {
            if write_frame(&mut writer, &message).await.is_err() {
                break;
            }
        }
    };
    // Anything but end-of-stream is unexpected; either way, it's gone.
    let read = async {
        let _ = read_frame::<_, IpcMessage>(&mut reader).await;
    };
    tokio::select! {
        () = write => {}
        () = read => {}
    }
    if let Some(bridge) = &bridge {
        bridge.detach_injector(&to_injector);
    }
    info!("input helper disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::time::{timeout, Duration};

    fn status(agent_id: Option<&str>, connected: bool) -> AgentStatus {
        AgentStatus {
            agent_id: agent_id.map(str::to_owned),
            connected,
            version: "test".into(),
        }
    }

    async fn next_status(
        pipe: &mut tokio::net::windows::named_pipe::NamedPipeClient,
    ) -> AgentStatus {
        match timeout(Duration::from_secs(5), read_frame::<_, IpcMessage>(pipe))
            .await
            .expect("status within 5s")
            .unwrap()
        {
            Some(IpcMessage::Status(s)) => s,
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn helper_receives_current_status_and_every_change() {
        let name = format!(r"\\.\pipe\rmm-agent-test-{}", std::process::id());
        let (tx, rx) = watch::channel(status(Some("agt-1"), false));
        // This test process plays the helper, so its own pid is "ours".
        let pids = HelperPids {
            helper: Arc::new(AtomicU32::new(std::process::id())),
            ..HelperPids::default()
        };
        tokio::spawn({
            let name = name.clone();
            async move { serve_on(&name, rx, pids, None).await }
        });

        let mut client = loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        write_frame(
            &mut client,
            &IpcMessage::HelperHello {
                pid: std::process::id(),
                session_id: 1,
                version: "test".into(),
            },
        )
        .await
        .unwrap();

        assert_eq!(next_status(&mut client).await, status(Some("agt-1"), false));
        tx.send_modify(|s| s.connected = true);
        assert_eq!(next_status(&mut client).await, status(Some("agt-1"), true));
    }

    #[tokio::test]
    async fn clients_other_than_the_helper_are_disconnected() {
        let name = format!(r"\\.\pipe\rmm-agent-test-other-{}", std::process::id());
        let (_tx, rx) = watch::channel(status(None, false));
        let pids = HelperPids {
            helper: Arc::new(AtomicU32::new(1)), // not us
            input: Arc::new(AtomicU32::new(2)),  // nor this
        };
        tokio::spawn({
            let name = name.clone();
            async move { serve_on(&name, rx, pids, None).await }
        });
        let mut client = loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        let _ = write_frame(
            &mut client,
            &IpcMessage::HelperHello {
                pid: std::process::id(),
                session_id: 1,
                version: "test".into(),
            },
        )
        .await;
        let got = timeout(
            Duration::from_secs(5),
            read_frame::<_, IpcMessage>(&mut client),
        )
        .await
        .expect("server should hang up");
        assert!(!matches!(got, Ok(Some(_))), "impostor got {got:?}");
    }

    #[tokio::test]
    async fn input_goes_to_the_input_helper_when_one_is_attached() {
        use crate::interactive::{desktop_channel, DesktopCommand};
        use crate::media::source::media_channel;
        use protocol::input::InputEvent;

        let name = format!(r"\\.\pipe\rmm-agent-test-input-{}", std::process::id());
        let (_tx, rx) = watch::channel(status(None, false));
        let (_media_link, media_source) = media_channel();
        let (desktop_link, desktop_end) = desktop_channel(|| true);
        let bridge = Bridge::start(media_source, desktop_end);
        // This test process plays the input helper.
        let pids = HelperPids {
            input: Arc::new(AtomicU32::new(std::process::id())),
            ..HelperPids::default()
        };
        tokio::spawn({
            let name = name.clone();
            async move { serve_on(&name, rx, pids, Some(bridge)).await }
        });
        let mut client = loop {
            match ClientOptions::new().open(&name) {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        write_frame(
            &mut client,
            &IpcMessage::HelperHello {
                pid: std::process::id(),
                session_id: 1,
                version: "test".into(),
            },
        )
        .await
        .unwrap();

        let key = InputEvent::Key {
            scancode: 0x1E,
            down: true,
        };
        // The bridge attaches the input helper after reading the hello, and
        // input sent before that goes nowhere: resend until one arrives.
        let mut reader =
            tokio::spawn(async move { read_frame::<_, IpcMessage>(&mut client).await });
        let got = timeout(Duration::from_secs(5), async {
            loop {
                desktop_link
                    .commands
                    .send(DesktopCommand::Input(key))
                    .unwrap();
                tokio::select! {
                    got = &mut reader => break got.unwrap(),
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        })
        .await
        .expect("input within 5s");
        assert_eq!(got.unwrap(), Some(IpcMessage::Input(key)));
    }
}
