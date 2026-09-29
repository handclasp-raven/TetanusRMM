//! Screen capture with the DXGI Desktop Duplication API.
//!
//! Runs in the helper, inside the user's session: duplication only works on
//! the desktop the calling process is attached to (see `crate::session`).
//!
//! DXGI reports which regions changed since the last frame (dirty rects, plus
//! "move" rects for scrolled/dragged content). Only those regions are copied
//! off the GPU and converted to NV12, so an idle or mostly-idle screen costs
//! very little CPU. The staging texture and NV12 frame persist between frames
//! and always hold the full current image.

use protocol::media::MonitorInfo;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT,
};

use crate::media::nv12::{Nv12Frame, Rect};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The duplication became invalid: desktop switch (UAC, lock screen),
    /// mode change, or another full-screen app. Recreate the duplicator.
    #[error("desktop duplication access lost")]
    AccessLost,
    #[error("no monitor with id {0}")]
    NoSuchMonitor(u32),
    #[error(transparent)]
    Windows(#[from] windows::core::Error),
}

struct Output {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    info: MonitorInfo,
}

fn outputs() -> windows::core::Result<Vec<Output>> {
    // SAFETY: plain COM calls; all out-values are owned wrappers.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut found = Vec::new();
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                let name_len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                let info = MonitorInfo {
                    id: found.len() as u32,
                    name: String::from_utf16_lossy(&desc.DeviceName[..name_len]),
                    x: r.left,
                    y: r.top,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                    // The primary monitor is the one at the desktop origin.
                    primary: r.left == 0 && r.top == 0,
                };
                found.push(Output {
                    adapter: adapter.clone(),
                    output: output.cast()?,
                    info,
                });
            }
        }
        Ok(found)
    }
}

/// Monitors attached to the desktop, in adapter/output order. Ids are indexes
/// into this list.
pub fn monitors() -> windows::core::Result<Vec<MonitorInfo>> {
    Ok(outputs()?.into_iter().map(|o| o.info).collect())
}

/// What happened on one [`Duplicator::next_frame`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Captured {
    /// Nothing changed on screen within the timeout.
    Unchanged,
    /// The NV12 frame was updated; `pixels` is the changed area.
    Updated { pixels: u64, rects: usize },
}

pub struct Duplicator {
    pub monitor: MonitorInfo,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    frame: Nv12Frame,
    /// First frame: copy everything, not just dirty rects.
    first: bool,
    metadata: Vec<u8>,
}

impl Duplicator {
    pub fn new(monitor_id: u32) -> Result<Self, CaptureError> {
        let out = outputs()?
            .into_iter()
            .find(|o| o.info.id == monitor_id)
            .ok_or(CaptureError::NoSuchMonitor(monitor_id))?;
        // SAFETY: COM calls with owned out-parameters.
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            // The device must live on the adapter that owns the output.
            D3D11CreateDevice(
                &out.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.expect("device on success");
            let context = context.expect("context on success");
            let duplication = out.output.DuplicateOutput(&device)?;
            let mode = duplication.GetDesc().ModeDesc;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: mode.Width,
                Height: mode.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: mode.Format,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
            let mut monitor = out.info;
            // The mode can differ from the desktop rect under DPI/rotation.
            monitor.width = mode.Width;
            monitor.height = mode.Height;
            Ok(Self {
                frame: Nv12Frame::new(mode.Width, mode.Height),
                monitor,
                context,
                duplication,
                staging: staging.expect("texture on success"),
                first: true,
                metadata: Vec::new(),
            })
        }
    }

    /// The current full image.
    pub fn frame(&self) -> &Nv12Frame {
        &self.frame
    }

    /// Wait up to `timeout_ms` for the screen to change, and fold the changed
    /// regions into [`Self::frame`].
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Captured, CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out-parameters owned here; the frame is released below.
        match unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Captured::Unchanged),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(CaptureError::AccessLost),
            Err(e) => return Err(e.into()),
        }
        let result = self.process(&info, resource);
        // SAFETY: a frame was acquired above.
        match unsafe { self.duplication.ReleaseFrame() } {
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(CaptureError::AccessLost),
            _ => {}
        }
        result
    }

    fn process(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<Captured, CaptureError> {
        // Only the mouse moved: no new desktop image.
        if info.LastPresentTime == 0 && !self.first {
            return Ok(Captured::Unchanged);
        }
        let Some(resource) = resource else {
            return Ok(Captured::Unchanged);
        };
        let texture: ID3D11Texture2D = resource.cast()?;
        let (w, h) = (self.frame.width, self.frame.height);
        let rects = if self.first {
            vec![Rect::full(w, h)]
        } else {
            self.changed_rects(info.TotalMetadataBufferSize)?
        };
        if rects.is_empty() {
            return Ok(Captured::Unchanged);
        }

        // SAFETY: both textures are alive; boxes are within bounds (aligned).
        unsafe {
            for r in &rects {
                let b = D3D11_BOX {
                    left: r.left,
                    top: r.top,
                    front: 0,
                    right: r.right,
                    bottom: r.bottom,
                    back: 1,
                };
                self.context.CopySubresourceRegion(
                    &self.staging,
                    0,
                    r.left,
                    r.top,
                    0,
                    &texture,
                    0,
                    Some(&b),
                );
            }
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let pitch = mapped.RowPitch as usize;
            let bgra = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h as usize);
            for r in &rects {
                self.frame.update_from_bgra(bgra, pitch, *r);
            }
            self.context.Unmap(&self.staging, 0);
        }
        self.first = false;
        Ok(Captured::Updated {
            pixels: rects.iter().map(Rect::area).sum(),
            rects: rects.len(),
        })
    }

    /// Dirty rects plus the destinations of move rects, clipped and aligned.
    fn changed_rects(&mut self, metadata_size: u32) -> Result<Vec<Rect>, CaptureError> {
        let (w, h) = (self.frame.width, self.frame.height);
        let to_rect = |r: RECT| Rect {
            left: r.left.max(0) as u32,
            top: r.top.max(0) as u32,
            right: r.right.max(0) as u32,
            bottom: r.bottom.max(0) as u32,
        };
        self.metadata.resize(metadata_size.max(1) as usize, 0);
        let mut rects = Vec::new();
        // SAFETY: the buffer is metadata_size bytes, as DXGI requires; the
        // returned sizes say how many entries were written.
        unsafe {
            let mut used = 0u32;
            self.duplication.GetFrameMoveRects(
                self.metadata.len() as u32,
                self.metadata.as_mut_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(),
                &mut used,
            )?;
            let moves = std::slice::from_raw_parts(
                self.metadata.as_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(),
                used as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>(),
            );
            rects.extend(moves.iter().map(|m| to_rect(m.DestinationRect)));

            let mut used = 0u32;
            self.duplication.GetFrameDirtyRects(
                self.metadata.len() as u32,
                self.metadata.as_mut_ptr().cast::<RECT>(),
                &mut used,
            )?;
            let dirty = std::slice::from_raw_parts(
                self.metadata.as_ptr().cast::<RECT>(),
                used as usize / std::mem::size_of::<RECT>(),
            );
            rects.extend(dirty.iter().map(|r| to_rect(*r)));
        }
        Ok(rects
            .into_iter()
            .filter_map(|r| r.aligned_within(w, h))
            .collect())
    }
}
