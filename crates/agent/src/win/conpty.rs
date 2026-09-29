//! PowerShell on a Windows pseudoconsole (ConPTY).
//!
//! The pseudoconsole turns the console API calls of the programs attached to
//! it into a VT byte stream on one pipe, and parses VT input from another,
//! so the technician's terminal renders PowerShell exactly as a local
//! Windows Terminal would.
//!
//! Under the service this runs as **LocalSystem in session 0**: the shell
//! has full control of the machine and no access to the logged-on user's
//! desktop or drive mappings. That is the usual RMM model (and works with
//! nobody logged on), but everything typed runs as SYSTEM.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::Mutex;

use protocol::shell::TermSize;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::remote::pty::{PtyProcess, Spawned};

fn coord(size: TermSize) -> COORD {
    // TermSize is at most 1000 in each dimension, well within i16.
    COORD {
        X: size.cols as i16,
        Y: size.rows as i16,
    }
}

fn io_error(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(e)
}

/// Closes a raw handle on drop unless taken.
struct Handle(HANDLE);

impl Handle {
    fn into_file(self) -> File {
        let raw = self.0 .0;
        std::mem::forget(self);
        // SAFETY: we own the handle and hand ownership to the File.
        File::from(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// An initialized attribute list holding one pseudoconsole attribute.
struct AttributeList(Vec<u8>);

impl AttributeList {
    fn with_pseudoconsole(hpc: HPCON) -> windows::core::Result<Self> {
        let mut size = 0;
        // SAFETY: size query; this "fails" with ERROR_INSUFFICIENT_BUFFER by design.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        let mut list = Self(vec![0u8; size]);
        // SAFETY: the buffer is `size` bytes as requested.
        unsafe { InitializeProcThreadAttributeList(Some(list.ptr()), 1, None, &mut size)? };
        // SAFETY: the attribute value is the HPCON itself (not a pointer to it),
        // as the ConPTY documentation specifies.
        unsafe {
            UpdateProcThreadAttribute(
                list.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc.0 as *const c_void),
                std::mem::size_of::<HPCON>(),
                None,
                None,
            )?
        };
        Ok(list)
    }

    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.0.as_mut_ptr().cast())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialized in `with_pseudoconsole`.
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

struct ConPty {
    /// Process handle, as an integer so the struct is Send + Sync.
    process: isize,
    /// `None` once closed.
    console: Mutex<Option<HPCON>>,
}

impl ConPty {
    fn process(&self) -> HANDLE {
        HANDLE(self.process as *mut c_void)
    }
}

impl PtyProcess for ConPty {
    fn resize(&self, size: TermSize) -> std::io::Result<()> {
        let console = self.console.lock().unwrap_or_else(|e| e.into_inner());
        match *console {
            // SAFETY: open pseudoconsole handle.
            Some(hpc) => unsafe { ResizePseudoConsole(hpc, coord(size)) }.map_err(io_error),
            None => Ok(()),
        }
    }

    fn wait(&self) -> Option<i32> {
        // SAFETY: valid process handle for our lifetime.
        unsafe {
            if WaitForSingleObject(self.process(), INFINITE) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0u32;
            GetExitCodeProcess(self.process(), &mut code).ok()?;
            Some(code as i32)
        }
    }

    fn kill(&self) {
        // SAFETY: valid process handle; fails harmlessly if already exited.
        unsafe {
            let _ = TerminateProcess(self.process(), 1);
        }
    }

    fn close(&self) {
        let hpc = self
            .console
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(hpc) = hpc {
            // Ends conhost, which closes the output pipe (and ends any
            // console programs the shell left behind). Blocks until pending
            // output is read on older Windows builds; the output thread keeps
            // draining it.
            // SAFETY: open pseudoconsole handle, closed once.
            unsafe { ClosePseudoConsole(hpc) };
        }
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        self.close();
        // SAFETY: we own the process handle.
        unsafe {
            let _ = CloseHandle(self.process());
        }
    }
}

/// `%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`, by full
/// path so a SYSTEM process never picks up a planted `powershell.exe`.
pub fn powershell_path() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::path::Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe")
}

/// Start PowerShell on a new pseudoconsole of `size`.
pub fn spawn_powershell(size: TermSize) -> std::io::Result<Spawned> {
    // Two anonymous pipes: we write the shell's input into one and read its
    // output from the other. The far ends belong to the pseudoconsole.
    let (mut in_read, mut in_write) = (HANDLE::default(), HANDLE::default());
    let (mut out_read, mut out_write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: out-parameters; each handle is owned by a guard right after.
    unsafe { CreatePipe(&mut in_read, &mut in_write, None, 0) }.map_err(io_error)?;
    let (in_read, in_write) = (Handle(in_read), Handle(in_write));
    // SAFETY: as above.
    unsafe { CreatePipe(&mut out_read, &mut out_write, None, 0) }.map_err(io_error)?;
    let (out_read, out_write) = (Handle(out_read), Handle(out_write));

    // SAFETY: valid pipe handles; the pseudoconsole duplicates what it needs.
    let hpc =
        unsafe { CreatePseudoConsole(coord(size), in_read.0, out_write.0, 0) }.map_err(io_error)?;
    let conpty = ConPtyGuard(Some(hpc));
    // conhost has its own copies now; ours would keep the output pipe open
    // after it exits.
    drop((in_read, out_write));

    let mut attributes = AttributeList::with_pseudoconsole(hpc).map_err(io_error)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Explicitly no standard handles, so the shell cannot inherit the
    // service's (redirected) ones instead of attaching to the pseudoconsole.
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    startup.lpAttributeList = attributes.ptr();

    let exe = powershell_path();
    let mut command_line: Vec<u16> = format!("\"{}\" -NoLogo", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let exe_wide: Vec<u16> = exe
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: every pointer references a local that outlives the call; the
    // command line buffer is mutable as CreateProcessW requires.
    unsafe {
        CreateProcessW(
            PCWSTR(exe_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &startup.StartupInfo,
            &mut info,
        )
    }
    .map_err(io_error)?;
    drop(Handle(info.hThread));
    drop(attributes);

    Ok(Spawned {
        output: Box::new(out_read.into_file()),
        input: Box::new(in_write.into_file()),
        process: Box::new(ConPty {
            process: info.hProcess.0 as isize,
            console: Mutex::new(conpty.take()),
        }),
    })
}

/// Closes the pseudoconsole if spawning fails part-way.
struct ConPtyGuard(Option<HPCON>);

impl ConPtyGuard {
    fn take(mut self) -> Option<HPCON> {
        self.0.take()
    }
}

impl Drop for ConPtyGuard {
    fn drop(&mut self) {
        if let Some(hpc) = self.0.take() {
            // SAFETY: open pseudoconsole handle.
            unsafe { ClosePseudoConsole(hpc) };
        }
    }
}
