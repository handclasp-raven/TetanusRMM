//! Service side of the helper pipe.
//!
//! Access control, in layers:
//! 1. The pipe's DACL lets only SYSTEM, Administrators and interactively
//!    logged-on users (IU) connect, and remote clients are rejected.
//! 2. The service creates the first instance with `first_pipe_instance`, so
//!    if another process already owns the name, creation fails instead of
//!    the service unknowingly sharing it.
//! 3. Each client's process id must be the helper the service itself just
//!    spawned; anything else is disconnected.

use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use protocol::ipc::{AgentStatus, IpcMessage, PIPE_NAME};
use protocol::{read_frame, write_frame};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::watch;
use tracing::{info, warn};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;

use super::acl::SecurityDescriptor;

/// SYSTEM and Administrators: full. Interactive users: read/write.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

fn client_pid(pipe: &NamedPipeServer) -> Option<u32> {
    let mut pid = 0;
    // SAFETY: the raw handle is valid for the lifetime of `pipe`.
    unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid).ok()? };
    Some(pid)
}

/// Accept helper connections forever. `helper_pid` is the pid of the helper
/// the supervisor last spawned (0 if none).
pub async fn serve(
    status: watch::Receiver<AgentStatus>,
    helper_pid: Arc<AtomicU32>,
) -> std::io::Result<()> {
    serve_on(PIPE_NAME, status, helper_pid).await
}

/// [`serve`] on a given pipe name (tests use a unique one).
pub async fn serve_on(
    pipe_name: &str,
    status: watch::Receiver<AgentStatus>,
    helper_pid: Arc<AtomicU32>,
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

        let expected = helper_pid.load(Ordering::SeqCst);
        match client_pid(&server) {
            Some(pid) if pid == expected && pid != 0 => {
                info!(pid, "helper connected");
                tokio::spawn(serve_helper(server, status.clone()));
            }
            other => {
                warn!(client = ?other, expected, "rejected pipe client that is not our helper");
            }
        }
    }
}

async fn serve_helper(mut pipe: NamedPipeServer, mut status: watch::Receiver<AgentStatus>) {
    match read_frame::<_, IpcMessage>(&mut pipe).await {
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
    loop {
        let current = status.borrow_and_update().clone();
        if write_frame(&mut pipe, &IpcMessage::Status(current))
            .await
            .is_err()
        {
            break;
        }
        if status.changed().await.is_err() {
            break;
        }
    }
    info!("helper disconnected");
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
        let pid = Arc::new(AtomicU32::new(std::process::id()));
        tokio::spawn({
            let name = name.clone();
            async move { serve_on(&name, rx, pid).await }
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
        let pid = Arc::new(AtomicU32::new(1)); // not us
        tokio::spawn({
            let name = name.clone();
            async move { serve_on(&name, rx, pid).await }
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
}
