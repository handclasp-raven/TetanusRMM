//! The user's clipboard, from the helper: a message-only window registered
//! with `AddClipboardFormatListener` flags changes, and text or copied file
//! lists are read and written with the classic clipboard API. Everything
//! here runs on the helper's UI thread, which owns the window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::clipboard::ClipboardData;
use windows::core::{w, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};

/// Set by the window procedure; taken by [`Listener::changed`].
static CHANGED: AtomicBool = AtomicBool::new(false);

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        CHANGED.store(true, Ordering::SeqCst);
        return LRESULT(0);
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// The clipboard listener window. Create and use it on one thread, which
/// must pump messages.
pub struct Listener {
    hwnd: HWND,
}

impl Listener {
    pub fn new() -> Result<Self> {
        // SAFETY: registering a window class and creating a message-only
        // window with static strings and a valid window procedure.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("RmmAgentClipboard");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // Fails harmlessly if already registered.
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("RMM Agent clipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )?;
            AddClipboardFormatListener(hwnd)?;
            Ok(Self { hwnd })
        }
    }

    /// Whether the clipboard changed since the last call.
    pub fn changed(&self) -> bool {
        CHANGED.swap(false, Ordering::SeqCst)
    }

    /// The clipboard's text or file list, if it holds either.
    pub fn read(&self) -> Option<ClipboardData> {
        let _open = Open::new(self.hwnd)?;
        // SAFETY: the clipboard is open; handles are used only while it is.
        unsafe {
            if IsClipboardFormatAvailable(u32::from(CF_HDROP.0)).is_ok() {
                let handle = GetClipboardData(u32::from(CF_HDROP.0)).ok()?;
                let hdrop = HDROP(handle.0);
                let count = DragQueryFileW(hdrop, u32::MAX, None);
                let paths: Vec<String> = (0..count)
                    .map(|i| {
                        let len = DragQueryFileW(hdrop, i, None) as usize;
                        let mut buf = vec![0u16; len + 1];
                        let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                        String::from_utf16_lossy(&buf[..got])
                    })
                    .collect();
                return Some(ClipboardData::Files(paths));
            }
            if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_ok() {
                let handle = GetClipboardData(u32::from(CF_UNICODETEXT.0)).ok()?;
                let global = HGLOBAL(handle.0);
                let ptr = GlobalLock(global) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                // Bounded by the allocation, in case the text is not terminated.
                let max = GlobalSize(global) / 2;
                let units = std::slice::from_raw_parts(ptr, max);
                let len = units.iter().position(|&c| c == 0).unwrap_or(max);
                let text = String::from_utf16_lossy(&units[..len]);
                let _ = GlobalUnlock(global);
                return Some(ClipboardData::Text(text));
            }
        }
        None
    }

    /// Replace the clipboard with `data` (a file list is written as text:
    /// the files are on the technician's machine, not this one).
    pub fn write(&self, data: &ClipboardData) -> Result<()> {
        let units: Vec<u16> = data.to_text().encode_utf16().collect();
        let bytes = (units.len() + 1) * 2;
        let Some(_open) = Open::new(self.hwnd) else {
            return Err(windows::core::Error::from_thread());
        };
        // SAFETY: the clipboard is open and owned by our window; the
        // allocation is large enough for the text and its terminator, and
        // ownership passes to the clipboard on success.
        unsafe {
            EmptyClipboard()?;
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let ptr = GlobalLock(global) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return Err(windows::core::Error::from_thread());
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            *ptr.add(units.len()) = 0;
            let _ = GlobalUnlock(global);
            if let Err(e) = SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(global.0))) {
                let _ = GlobalFree(Some(global));
                return Err(e);
            }
        }
        Ok(())
    }
}

/// An open clipboard, closed on drop. Another program may hold the
/// clipboard briefly, so opening retries for a moment.
struct Open;

impl Open {
    fn new(hwnd: HWND) -> Option<Self> {
        for _ in 0..10 {
            // SAFETY: `hwnd` is our live window.
            if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
                return Some(Open);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: we opened it.
        let _ = unsafe { CloseClipboard() };
    }
}
