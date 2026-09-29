//! The agent's end of an interactive shell stream (see `protocol::shell`).
//!
//! The pseudoterminal APIs are blocking, so three threads serve each shell:
//! one reads its output, one writes its input, and one waits for it to exit.
//! This task relays between them and the stream. The shell dies with the
//! stream: when the server hangs up, or the connection drops and this task
//! is aborted, the process is killed.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use protocol::shell::{ShellInput, ShellOutput, TermSize};
use protocol::{read_frame, write_frame, FrameError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::pty::{self, PtyProcess, Spawned};

/// Output read per chunk (and so the largest `Data` frame the agent sends).
const READ_BUFFER: usize = 16 * 1024;

/// After the shell exits, how long to wait for output still in flight before
/// reporting the exit. Output can stay open if the shell left a background
/// program holding the terminal.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// Kills the shell when dropped, however the session ends.
struct KillOnDrop(Arc<dyn PtyProcess>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.0.kill();
    }
}

/// Run a shell for one stream until it exits or the server hangs up.
pub async fn serve<W, R>(size: TermSize, send: &mut W, recv: &mut R) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    if !size.is_valid() {
        let msg = ShellOutput::Error("invalid terminal size".into());
        return write_frame(send, &msg).await;
    }
    match pty::spawn(size) {
        Ok(spawned) => {
            info!(cols = size.cols, rows = size.rows, "shell started");
            relay(spawned, send, recv).await
        }
        Err(e) => {
            warn!("could not start shell: {e}");
            let msg = ShellOutput::Error(format!("could not start shell: {e}"));
            write_frame(send, &msg).await
        }
    }
}

/// Relay an already started shell (tests use this with a fake one).
pub async fn relay<W, R>(spawned: Spawned, send: &mut W, recv: &mut R) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
    R: AsyncRead + Unpin + Send,
{
    let Spawned {
        output,
        input,
        process,
    } = spawned;
    let process: Arc<dyn PtyProcess> = process.into();
    let _kill = KillOnDrop(process.clone());

    let mut output_rx = spawn_output_reader(output);
    let input_tx = spawn_input_writer(input);
    let mut exit_rx = spawn_waiter(process.clone());

    write_frame(send, &ShellOutput::Started).await?;

    // Terminal output to the server, then the exit code.
    let pump = async {
        let mut exit: Option<Option<i32>> = None;
        let drain_deadline = tokio::time::sleep(Duration::MAX);
        tokio::pin!(drain_deadline);
        loop {
            tokio::select! {
                chunk = output_rx.recv() => match chunk {
                    Some(bytes) => write_frame(send, &ShellOutput::Data(bytes)).await?,
                    None => break,
                },
                code = &mut exit_rx, if exit.is_none() => {
                    exit = Some(code.ok().flatten());
                    drain_deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + DRAIN_AFTER_EXIT);
                }
                () = &mut drain_deadline, if exit.is_some() => {
                    debug!("output still open after the shell exited; not waiting for it");
                    break;
                }
            }
        }
        let code = match exit {
            Some(code) => code,
            // Output ended first; the exit follows promptly.
            None => exit_rx.await.ok().flatten(),
        };
        info!(?code, "shell exited");
        write_frame(send, &ShellOutput::Exited { code }).await
    };

    // Keystrokes and resizes from the server. Ends when the server hangs up.
    let commands = async {
        loop {
            match read_frame::<_, ShellInput>(recv).await {
                Ok(Some(ShellInput::Data(bytes))) => {
                    if input_tx.send(bytes).is_err() {
                        debug!("shell input closed");
                    }
                }
                Ok(Some(ShellInput::Resize(size))) if size.is_valid() => {
                    debug!(cols = size.cols, rows = size.rows, "resize");
                    if let Err(e) = process.resize(size) {
                        warn!("resizing the terminal failed: {e}");
                    }
                }
                Ok(Some(ShellInput::Resize(_))) => warn!("invalid terminal size ignored"),
                Ok(None) => return,
                Err(e) => {
                    warn!("shell stream from server failed: {e}");
                    return;
                }
            }
        }
    };

    tokio::select! {
        result = pump => result,
        () = commands => {
            info!("server hung up; killing the shell");
            Ok(())
        }
    }
}

/// Reads output on a thread. Keeps reading (and discarding) if nobody is
/// listening any more, so the terminal never blocks on a full pipe.
fn spawn_output_reader(mut output: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(16);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; READ_BUFFER];
        let mut listening = true;
        // Errors (EIO on Unix once the shell is gone, a broken pipe on
        // Windows) mean end of output.
        while let Ok(n @ 1..) = output.read(&mut buf) {
            if listening && tx.blocking_send(buf[..n].to_vec()).is_err() {
                listening = false;
            }
        }
    });
    rx
}

/// Writes input on a thread, in order. Ends when the sender is dropped.
fn spawn_input_writer(mut input: Box<dyn Write + Send>) -> std::sync::mpsc::Sender<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for bytes in rx {
            if input
                .write_all(&bytes)
                .and_then(|()| input.flush())
                .is_err()
            {
                break;
            }
        }
    });
    tx
}

/// Waits for the shell to exit on a thread, then releases the terminal.
fn spawn_waiter(process: Arc<dyn PtyProcess>) -> oneshot::Receiver<Option<i32>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let code = process.wait();
        process.close();
        let _ = tx.send(code);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};

    /// A fake shell: echoes input to output in upper case, records resizes,
    /// and exits with the code typed after "exit ".
    #[derive(Default)]
    struct Fake {
        resizes: Mutex<Vec<TermSize>>,
        exit: Mutex<Option<Option<i32>>>,
        exited: Condvar,
        killed: AtomicBool,
    }

    impl Fake {
        fn finish(&self, code: Option<i32>) {
            *self.exit.lock().unwrap() = Some(code);
            self.exited.notify_all();
        }
    }

    impl PtyProcess for Arc<Fake> {
        fn resize(&self, size: TermSize) -> std::io::Result<()> {
            self.resizes.lock().unwrap().push(size);
            Ok(())
        }
        fn wait(&self) -> Option<i32> {
            let mut exit = self.exit.lock().unwrap();
            loop {
                if let Some(code) = *exit {
                    return code;
                }
                exit = self.exited.wait(exit).unwrap();
            }
        }
        fn kill(&self) {
            self.killed.store(true, Ordering::SeqCst);
            self.finish(None);
        }
        fn close(&self) {}
    }

    /// Output side of the fake terminal, fed by the input side.
    struct Pipe(std::sync::mpsc::Receiver<Vec<u8>>, Vec<u8>);

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.1.is_empty() {
                match self.0.recv() {
                    Ok(bytes) => self.1 = bytes,
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.1.len());
            buf[..n].copy_from_slice(&self.1[..n]);
            self.1.drain(..n);
            Ok(n)
        }
    }

    struct Keyboard(std::sync::mpsc::Sender<Vec<u8>>, Arc<Fake>);

    impl Write for Keyboard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let text = String::from_utf8_lossy(buf).to_string();
            if let Some(code) = text.trim().strip_prefix("exit ") {
                self.1.finish(code.parse().ok());
                // The terminal closes with the shell.
                let _ = self.0.send(Vec::new());
                self.0 = std::sync::mpsc::channel().0;
            } else {
                let _ = self.0.send(text.to_uppercase().into_bytes());
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fake() -> (Spawned, Arc<Fake>) {
        let fake = Arc::new(Fake::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = Spawned {
            output: Box::new(Pipe(rx, Vec::new())),
            input: Box::new(Keyboard(tx, fake.clone())),
            process: Box::new(fake.clone()),
        };
        (spawned, fake)
    }

    async fn next(r: &mut tokio::io::DuplexStream) -> ShellOutput {
        tokio::time::timeout(Duration::from_secs(5), read_frame(r))
            .await
            .expect("frame in time")
            .unwrap()
            .expect("stream open")
    }

    #[tokio::test]
    async fn relays_input_output_resize_and_exit() {
        let (spawned, fake) = fake();
        let (mut agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        let (mut server_send, mut agent_recv) = tokio::io::duplex(1 << 16);
        let agent =
            tokio::spawn(async move { relay(spawned, &mut agent_send, &mut agent_recv).await });

        assert_eq!(next(&mut server_recv).await, ShellOutput::Started);
        write_frame(&mut server_send, &ShellInput::Data(b"dir".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            next(&mut server_recv).await,
            ShellOutput::Data(b"DIR".to_vec())
        );
        let size = TermSize {
            cols: 200,
            rows: 50,
        };
        write_frame(&mut server_send, &ShellInput::Resize(size))
            .await
            .unwrap();
        // An invalid size never reaches the terminal.
        write_frame(
            &mut server_send,
            &ShellInput::Resize(TermSize { cols: 0, rows: 50 }),
        )
        .await
        .unwrap();
        write_frame(&mut server_send, &ShellInput::Data(b"exit 7".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            next(&mut server_recv).await,
            ShellOutput::Exited { code: Some(7) }
        );
        agent.await.unwrap().unwrap();
        assert_eq!(*fake.resizes.lock().unwrap(), [size]);
    }

    #[tokio::test]
    async fn server_hang_up_kills_the_shell() {
        let (spawned, fake) = fake();
        let (mut agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        let (server_send, mut agent_recv) = tokio::io::duplex(1 << 16);
        let agent =
            tokio::spawn(async move { relay(spawned, &mut agent_send, &mut agent_recv).await });
        assert_eq!(next(&mut server_recv).await, ShellOutput::Started);
        drop(server_send);
        agent.await.unwrap().unwrap();
        assert!(fake.killed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_aborted_session_kills_the_shell() {
        let (spawned, fake) = fake();
        let (mut agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        let (_server_send, mut agent_recv) = tokio::io::duplex(1 << 16);
        let agent =
            tokio::spawn(async move { relay(spawned, &mut agent_send, &mut agent_recv).await });
        assert_eq!(next(&mut server_recv).await, ShellOutput::Started);
        // The connection dropped: the task running the session is aborted.
        agent.abort();
        let _ = agent.await;
        assert!(fake.killed.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_pty_shell_resizes_and_reports_its_exit_code() {
        let (mut agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        let (mut server_send, mut agent_recv) = tokio::io::duplex(1 << 16);
        let agent = tokio::spawn(async move {
            serve(
                TermSize { cols: 80, rows: 24 },
                &mut agent_send,
                &mut agent_recv,
            )
            .await
        });
        assert_eq!(next(&mut server_recv).await, ShellOutput::Started);
        write_frame(
            &mut server_send,
            &ShellInput::Resize(TermSize {
                cols: 132,
                rows: 43,
            }),
        )
        .await
        .unwrap();
        write_frame(
            &mut server_send,
            &ShellInput::Data(b"stty size; exit 3\n".to_vec()),
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        let code = loop {
            match next(&mut server_recv).await {
                ShellOutput::Data(bytes) => output.extend(bytes),
                ShellOutput::Exited { code } => break code,
                other => panic!("unexpected {other:?}"),
            }
        };
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("43 132"), "stty saw the resize: {text:?}");
        assert_eq!(code, Some(3));
        agent.await.unwrap().unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_real_conpty_powershell_resizes_and_reports_its_exit_code() {
        let (mut agent_send, mut server_recv) = tokio::io::duplex(1 << 16);
        let (mut server_send, mut agent_recv) = tokio::io::duplex(1 << 16);
        let agent = tokio::spawn(async move {
            serve(
                TermSize { cols: 80, rows: 24 },
                &mut agent_send,
                &mut agent_recv,
            )
            .await
        });
        assert_eq!(next(&mut server_recv).await, ShellOutput::Started);
        write_frame(
            &mut server_send,
            &ShellInput::Resize(TermSize {
                cols: 132,
                rows: 43,
            }),
        )
        .await
        .unwrap();
        // Give conhost a moment to apply the resize before asking.
        tokio::time::sleep(Duration::from_millis(500)).await;
        write_frame(
            &mut server_send,
            &ShellInput::Data(
                b"Write-Output \"size=$([Console]::WindowWidth)x$([Console]::WindowHeight)\"; exit 3\r"
                    .to_vec(),
            ),
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        let code = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match next(&mut server_recv).await {
                    ShellOutput::Data(bytes) => output.extend(bytes),
                    ShellOutput::Exited { code } => break code,
                    other => panic!("unexpected {other:?}"),
                }
            }
        })
        .await
        .expect("PowerShell exits");
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("size=132x43"),
            "PowerShell saw the resize: {text:?}"
        );
        assert_eq!(code, Some(3));
        agent.await.unwrap().unwrap();
    }
}
