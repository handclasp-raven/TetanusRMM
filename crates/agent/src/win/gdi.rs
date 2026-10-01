//! Reading screen pixels through GDI, for [`super::capture`] on machines
//! whose desktop is rendered in software and that lack
//! Windows.Graphics.Capture (before Windows 10 1903; see `super::wgc`).
//!
//! Desktop Duplication tells the capture when and where the screen changed.
//! On a hardware GPU its image is also the one to read. Where the desktop is
//! composed in software (virtual machines' display adapters, servers
//! without a GPU), the duplicated image is not protected while it is held:
//! Windows is still finishing it when it is handed over, and starts on the
//! next frame in the same surface while it is being read. Fast-changing
//! content (a video) then arrives torn, the top of one frame over the bottom
//! of another. GDI reads the finished, composed desktop instead, so there
//! the pixels come from here.

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ, SRCCOPY,
};

use crate::media::nv12::Rect;

/// A monitor's pixels, kept in a 32-bit BGRA bitmap the size of the
/// monitor; [`ScreenReader::read`] refreshes regions of it from the screen.
pub struct ScreenReader {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: u32,
    height: u32,
    /// The monitor's top-left corner on the virtual screen.
    origin: (i32, i32),
}

// SAFETY: created and used on the capture thread only.
unsafe impl Send for ScreenReader {}

impl ScreenReader {
    /// For the `width` x `height` monitor at `origin` on the virtual screen
    /// (physical pixels: the helper is per-monitor DPI aware).
    pub fn new(origin: (i32, i32), width: u32, height: u32) -> windows::core::Result<Self> {
        // SAFETY: plain GDI calls; every handle is released in Drop (or
        // below, on failure).
        unsafe {
            let screen = GetDC(None::<HWND>);
            if screen.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            let memory = CreateCompatibleDC(Some(screen));
            if memory.is_invalid() {
                let e = windows::core::Error::from_thread();
                ReleaseDC(None::<HWND>, screen);
                return Err(e);
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    // Negative: top-down rows, like a texture.
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(bitmap) => bitmap,
                    Err(e) => {
                        let _ = DeleteDC(memory);
                        ReleaseDC(None::<HWND>, screen);
                        return Err(e);
                    }
                };
            let previous = SelectObject(memory, bitmap.into());
            Ok(Self {
                screen,
                memory,
                bitmap,
                previous,
                bits: bits.cast(),
                width,
                height,
                origin,
            })
        }
    }

    /// Copy `rects` (monitor coordinates, within the monitor) from the
    /// screen; returns the whole bitmap as BGRA rows, and its pitch.
    pub fn read(&mut self, rects: &[Rect]) -> windows::core::Result<(&[u8], usize)> {
        // SAFETY: the rects are within the bitmap (the capture clips them
        // to the monitor), and the bitmap stays selected into `memory`.
        unsafe {
            for r in rects {
                BitBlt(
                    self.memory,
                    r.left as i32,
                    r.top as i32,
                    (r.right - r.left) as i32,
                    (r.bottom - r.top) as i32,
                    Some(self.screen),
                    self.origin.0 + r.left as i32,
                    self.origin.1 + r.top as i32,
                    SRCCOPY,
                )?;
            }
            // BitBlt may batch; the bits must be written before reading.
            let _ = GdiFlush();
            let pitch = self.width as usize * 4;
            let len = pitch * self.height as usize;
            Ok((std::slice::from_raw_parts(self.bits, len), pitch))
        }
    }
}

impl Drop for ScreenReader {
    fn drop(&mut self) {
        // SAFETY: our own handles, released once.
        unsafe {
            SelectObject(self.memory, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.memory);
            ReleaseDC(None::<HWND>, self.screen);
        }
    }
}
