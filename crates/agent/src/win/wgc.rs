//! Screen capture with Windows.Graphics.Capture, for [`super::capture`] on
//! machines whose desktop is rendered in software.
//!
//! Desktop Duplication hands over the one surface Windows composes into, and
//! on a software-rendered desktop (virtual machines' display adapters,
//! servers without a GPU) that surface is not protected while it is held:
//! fast-changing content arrives torn. Windows.Graphics.Capture instead
//! copies each composed frame into a pool of our own textures, so a frame
//! being read is never written to. It also reports which regions changed
//! (Windows 11 24H2 and later; before that every frame counts as changed).
//!
//! Needs Windows 10 1903 or later (`CreateForMonitor`); where it cannot
//! start, the capture falls back to duplication with GDI reads.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{factory, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess,
    GraphicsCaptureAccessKind, GraphicsCaptureDirtyRegionMode, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use super::capture::CaptureError;
use crate::media::nv12::Rect;

/// Textures in the pool: one being read, one being filled.
const BUFFERS: i32 = 2;

/// Set from the pool's and item's event threads.
#[derive(Default)]
struct Signals {
    /// A frame arrived since the pool was last drained.
    arrived: bool,
    /// The monitor went away.
    closed: bool,
}

type Shared = Arc<(Mutex<Signals>, Condvar)>;

/// A monitor's capture session.
pub struct GraphicsCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    signals: Shared,
    width: u32,
    height: u32,
}

/// The newest frame, and what changed since the previous one taken.
pub struct Frame {
    /// Returned to the pool when dropped.
    frame: Direct3D11CaptureFrame,
    pub texture: ID3D11Texture2D,
    /// `None`: not reported, treat the whole frame as changed.
    pub dirty: Option<Vec<Rect>>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

impl GraphicsCapture {
    /// Capture `monitor` with textures on `device`.
    pub fn new(device: &ID3D11Device, monitor: HMONITOR) -> windows::core::Result<Self> {
        // SAFETY: plain WinRT/COM calls; out-values are owned wrappers.
        unsafe {
            // Already initialised (the encoder shares this thread) is fine.
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
            let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
            let dxgi: IDXGIDevice = device.cast()?;
            let device: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                BUFFERS,
                size,
            )?;
            let session = pool.CreateCaptureSession(&item)?;

            let signals: Shared = Arc::default();
            let s = signals.clone();
            pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
                s.0.lock().expect("signals").arrived = true;
                s.1.notify_all();
                Ok(())
            }))?;
            let s = signals.clone();
            item.Closed(&TypedEventHandler::new(move |_, _| {
                s.0.lock().expect("signals").closed = true;
                s.1.notify_all();
                Ok(())
            }))?;

            // The viewer draws its own cursor; DXGI and GDI leave it out too.
            session.SetIsCursorCaptureEnabled(false)?;
            // The yellow capture border (Windows 11). The session indicator
            // already tells the user; unpackaged apps are granted this.
            let borderless =
                GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                    .and_then(|op| op.join());
            if let Err(e) = borderless.and_then(|_| session.SetIsBorderRequired(false)) {
                tracing::debug!("cannot hide the capture border: {e}");
            }
            // Report the changed regions (24H2+); frames still hold the whole
            // image.
            if let Err(e) = session.SetDirtyRegionMode(GraphicsCaptureDirtyRegionMode::ReportOnly) {
                tracing::debug!("no dirty regions from Windows.Graphics.Capture: {e}");
            }
            session.StartCapture()?;
            Ok(Self {
                pool,
                session,
                signals,
                width: size.Width as u32,
                height: size.Height as u32,
            })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Wait up to `timeout` for a new frame. Frames that queued up since
    /// the last call are skipped, their changed regions folded into the
    /// newest one's.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.drain()? {
                return Ok(Some(frame));
            }
            let (lock, arrived) = &*self.signals;
            let mut signals = lock.lock().expect("signals");
            while !signals.arrived && !signals.closed {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return Ok(None);
                };
                signals = arrived.wait_timeout(signals, left).expect("signals").0;
            }
            if signals.closed {
                return Err(CaptureError::AccessLost);
            }
            signals.arrived = false;
        }
    }

    fn drain(&mut self) -> Result<Option<Frame>, CaptureError> {
        let (w, h) = (self.width, self.height);
        let mut newest: Option<Direct3D11CaptureFrame> = None;
        let mut dirty = Some(Vec::new());
        // An empty pool comes back as an error (a null frame).
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            let size = frame.ContentSize()?;
            if (size.Width as u32, size.Height as u32) != (w, h) {
                // Resolution change: start over with a new pool.
                let _ = frame.Close();
                return Err(CaptureError::AccessLost);
            }
            match (&mut dirty, frame.DirtyRegions()) {
                (Some(rects), Ok(regions)) => {
                    for i in 0..regions.Size()? {
                        let r = regions.GetAt(i)?;
                        let clip = |v: i32, max: u32| v.clamp(0, max as i32) as u32;
                        let rect = Rect {
                            left: clip(r.X, w),
                            top: clip(r.Y, h),
                            right: clip(r.X.saturating_add(r.Width), w),
                            bottom: clip(r.Y.saturating_add(r.Height), h),
                        };
                        rects.extend(rect.aligned_within(w, h));
                    }
                }
                _ => dirty = None,
            }
            if let Some(old) = newest.replace(frame) {
                let _ = old.Close();
            }
        }
        let Some(frame) = newest else {
            return Ok(None);
        };
        let texture = frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?;
        // SAFETY: COM call returning an owned interface.
        let texture: ID3D11Texture2D = unsafe { texture.GetInterface()? };
        Ok(Some(Frame {
            frame,
            texture,
            dirty,
        }))
    }
}

impl Drop for GraphicsCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
