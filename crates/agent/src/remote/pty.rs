//! A shell on a pseudoterminal, behind one interface for both platforms:
//!
//! - **Windows:** PowerShell on a ConPTY (`crate::win::conpty`), running as
//!   the agent's account (SYSTEM under the service).
//! - **Unix (development fallback):** `/bin/sh` on an `openpty` terminal
//!   ([`unix`]). It exists so the whole shell path, resize included, can be
//!   exercised on Linux and macOS.
//!
//! Both are blocking APIs; [`super::shell`] drives them from threads.

use std::io::{Read, Write};

use protocol::shell::TermSize;

/// A started shell: its terminal's output and input, and the process.
pub struct Spawned {
    pub output: Box<dyn Read + Send>,
    pub input: Box<dyn Write + Send>,
    pub process: Box<dyn PtyProcess>,
}

pub trait PtyProcess: Send + Sync {
    fn resize(&self, size: TermSize) -> std::io::Result<()>;
    /// Block until the shell exits; its exit code, if it has one.
    fn wait(&self) -> Option<i32>;
    /// Kill the shell. Harmless if it already exited.
    fn kill(&self);
    /// Release the terminal once the shell has exited. Output reaches
    /// end-of-file only after this on Windows, where the pseudoconsole
    /// holds the output pipe open.
    fn close(&self);
}

/// Start the platform's shell on a new pseudoterminal of `size`.
pub fn spawn(size: TermSize) -> std::io::Result<Spawned> {
    #[cfg(windows)]
    return crate::win::conpty::spawn_powershell(size);
    #[cfg(unix)]
    return unix::spawn_sh(size);
    #[cfg(not(any(windows, unix)))]
    {
        let _ = size;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no pseudoterminal on this platform",
        ))
    }
}

#[cfg(unix)]
pub mod unix {
    //! `/bin/sh` on an `openpty` pseudoterminal (development fallback).

    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};

    use protocol::shell::TermSize;

    use super::{PtyProcess, Spawned};

    fn winsize(size: TermSize) -> libc::winsize {
        libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    fn check(ret: libc::c_int) -> std::io::Result<libc::c_int> {
        if ret == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret)
        }
    }

    struct UnixPty {
        master: OwnedFd,
        /// Also the process group id: the shell leads its own session.
        pid: libc::pid_t,
        exited: AtomicBool,
    }

    impl PtyProcess for UnixPty {
        fn resize(&self, size: TermSize) -> std::io::Result<()> {
            let ws = winsize(size);
            // SAFETY: valid fd; TIOCSWINSZ reads a winsize.
            check(unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) })?;
            Ok(())
        }

        fn wait(&self) -> Option<i32> {
            let mut status = 0;
            loop {
                // SAFETY: waiting on our own child.
                let ret = unsafe { libc::waitpid(self.pid, &mut status, 0) };
                if ret == -1
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                self.exited.store(true, Ordering::SeqCst);
                if ret == -1 {
                    return None;
                }
                return libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
            }
        }

        fn kill(&self) {
            // Once reaped, the pid may belong to someone else.
            if !self.exited.load(Ordering::SeqCst) {
                // SAFETY: signals our own process group.
                unsafe { libc::kill(-self.pid, libc::SIGKILL) };
            }
        }

        fn close(&self) {}
    }

    pub fn spawn_sh(size: TermSize) -> std::io::Result<Spawned> {
        let (mut master, mut slave) = (-1, -1);
        let mut ws = winsize(size);
        // SAFETY: out-parameters; no name buffer or termios.
        check(unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut ws, // `*const` on Linux, `*mut` on macOS
            )
        })?;
        // SAFETY: openpty returned two fresh descriptors we now own.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // Keep the master out of the shell (openpty does not set CLOEXEC).
        // SAFETY: valid fd.
        check(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;

        let mut command = Command::new("/bin/sh");
        command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                // New session, with the terminal as its controlling tty.
                check(libc::setsid())?;
                check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        // `command` held the slave ends; they are closed now it is dropped,
        // so output ends when the shell (and anything it started) exits.
        drop(command);

        let output = File::from(master.try_clone()?);
        let input = File::from(master.try_clone()?);
        Ok(Spawned {
            output: Box::new(output),
            input: Box::new(input),
            process: Box::new(UnixPty {
                master,
                pid: child.id() as libc::pid_t,
                exited: AtomicBool::new(false),
            }),
        })
    }
}
