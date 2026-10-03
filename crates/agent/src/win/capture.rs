//! Screen capture.
//!
//! Runs in the helper, inside the user's session: capture only works on the
//! desktop the calling process is attached to (see `crate::session`).
//!
//! Windows reports which regions changed since the last frame. Only those
//! regions are copied and converted to NV12, so an idle or mostly-idle
//! screen costs very little CPU. The pixel store (staging texture or GDI
//! bitmap) and the NV12 frame persist between frames and always hold the
//! full current image.
//!
//! The API depends on the display adapter ([`Source`]): on a hardware GPU,
//! the DXGI Desktop Duplication API, whose image is copied off the GPU.
//! Where the desktop is composed in software (virtual machines, servers
//! without a GPU) the duplicated image can be read while Windows is still
//! writing it and arrives torn, so there Windows.Graphics.Capture is used
//! (see `super::wgc`); where that is unavailable (before Windows 10 1903),
//! duplication with the changed regions read through GDI (see `super::gdi`).

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
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT,
};
use windows::Win32::Graphics::Gdi::HMONITOR;

use super::gdi::ScreenReader;
use super::wgc::GraphicsCapture;
use crate::media::nv12::{Nv12Frame, Rect};

/// PCI vendors of hardware GPUs, whose duplicated images are safe to read.
const HARDWARE_GPU_VENDORS: [u32; 5] = [
    0x10DE, // NVIDIA
    0x1002, // AMD
    0x1022, // AMD
    0x8086, // Intel
    0x5143, // Qualcomm
];

/// How long to wait for Windows.Graphics.Capture's first frame.
const FIRST_FRAME_TIMEOUT_MS: u32 = 1000;

/// Where a [`ScreenCapture`] learns what changed and reads the pixels.
enum Source {
    /// Desktop Duplication: its change reports, and the pixels from
    /// `pixels`.
    Duplication {
        duplication: IDXGIOutputDuplication,
        pixels: PixelSource,
        metadata: Vec<u8>,
    },
    /// Windows.Graphics.Capture, through this staging texture.
    Graphics {
        capture: GraphicsCapture,
        staging: ID3D11Texture2D,
    },
}

/// Where duplication reads changed pixels.
enum PixelSource {
    /// The duplicated image, through this staging texture.
    Dxgi(ID3D11Texture2D),
    /// The composed desktop, through GDI.
    Gdi(ScreenReader),
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The duplication became invalid: desktop switch (UAC, lock screen),
    /// mode change, or another full-screen app. Recreate the capture.
    #[error("screen capture access lost")]
    AccessLost,
    #[error("no monitor with id {0}")]
    NoSuchMonitor(u32),
    #[error(transparent)]
    Windows(#[from] windows::core::Error),
}

struct Output {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    handle: HMONITOR,
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
                    handle: desc.Monitor,
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

/// What happened on one [`ScreenCapture::next_frame`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Captured {
    /// Nothing changed on screen within the timeout.
    Unchanged,
    /// The NV12 frame was updated; `pixels` is the changed area.
    Updated { pixels: u64, rects: usize },
}

pub struct ScreenCapture {
    pub monitor: MonitorInfo,
    context: ID3D11DeviceContext,
    source: Source,
    frame: Nv12Frame,
    /// First frame: copy everything, not just dirty rects.
    first: bool,
    /// How long copying and converting the last new frame took (the rest
    /// of `next_frame` is waiting for the screen to change).
    pub last_copy: std::time::Duration,
}

impl ScreenCapture {
    /// Capture `monitor_id`. `graphics_capture`: use Windows.Graphics.Capture
    /// where the adapter calls for it. The system helper does not: that
    /// API is for the user's own desktop, not the secure one.
    pub fn new(monitor_id: u32, graphics_capture: bool) -> Result<Self, CaptureError> {
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
            let adapter = out.adapter.GetDesc1()?;
            let name_len = adapter
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(adapter.Description.len());
            let adapter_name = String::from_utf16_lossy(&adapter.Description[..name_len]);
            let hardware = HARDWARE_GPU_VENDORS.contains(&adapter.VendorId);
            let mut monitor = out.info;

            let graphics = if hardware || !graphics_capture {
                None
            } else {
                match GraphicsCapture::new(&device, out.handle) {
                    Ok(capture) => Some(capture),
                    Err(e) => {
                        tracing::warn!("cannot use Windows.Graphics.Capture, duplicating: {e}");
                        None
                    }
                }
            };
            let (source, (width, height), name) = match graphics {
                Some(capture) => {
                    let size = capture.size();
                    let staging = staging_texture(&device, size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
                    (
                        Source::Graphics { capture, staging },
                        size,
                        "graphics-capture",
                    )
                }
                None => {
                    let duplication = out.output.DuplicateOutput(&device)?;
                    let mode = duplication.GetDesc().ModeDesc;
                    let size = (mode.Width, mode.Height);
                    // GDI and the duplicated image agree on coordinates
                    // unless the mode differs from the desktop rect
                    // (rotation).
                    let same_shape = (monitor.width, monitor.height) == size;
                    let gdi = if !hardware && same_shape {
                        match ScreenReader::new((monitor.x, monitor.y), size.0, size.1) {
                            Ok(reader) => Some(reader),
                            Err(e) => {
                                tracing::warn!(
                                    "cannot read the screen through GDI, using DXGI: {e}"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let (pixels, name) = match gdi {
                        Some(reader) => (PixelSource::Gdi(reader), "duplication+gdi"),
                        None => (
                            PixelSource::Dxgi(staging_texture(&device, size, mode.Format)?),
                            "duplication",
                        ),
                    };
                    let source = Source::Duplication {
                        duplication,
                        pixels,
                        metadata: Vec::new(),
                    };
                    (source, size, name)
                }
            };
            tracing::info!(
                adapter = %adapter_name,
                vendor = format_args!("{:#06x}", adapter.VendorId),
                device = format_args!("{:#06x}", adapter.DeviceId),
                source = name,
                "screen capture source"
            );
            // The captured size can differ from the desktop rect under
            // DPI/rotation.
            monitor.width = width;
            monitor.height = height;
            let mut capture = Self {
                frame: Nv12Frame::new(width, height),
                monitor,
                context,
                source,
                first: true,
                last_copy: std::time::Duration::ZERO,
            };
            // Duplication has the current image ready at once; this
            // delivers its first frame a little after starting. Wait for
            // it, or the first frame streamed would be the empty image.
            if matches!(capture.source, Source::Graphics { .. }) {
                capture.next_frame(FIRST_FRAME_TIMEOUT_MS)?;
            }
            Ok(capture)
        }
    }

    /// The current full image.
    pub fn frame(&self) -> &Nv12Frame {
        &self.frame
    }

    /// Wait up to `timeout_ms` for the screen to change, and fold the changed
    /// regions into [`Self::frame`].
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Captured, CaptureError> {
        let rects = match &self.source {
            Source::Duplication { .. } => self.next_duplicated(timeout_ms)?,
            Source::Graphics { .. } => self.next_graphics(timeout_ms)?,
        };
        Ok(match rects {
            Some(rects) => {
                self.first = false;
                Captured::Updated {
                    pixels: rects.iter().map(Rect::area).sum(),
                    rects: rects.len(),
                }
            }
            None => Captured::Unchanged,
        })
    }

    fn next_graphics(&mut self, timeout_ms: u32) -> Result<Option<Vec<Rect>>, CaptureError> {
        let Source::Graphics { capture, staging } = &mut self.source else {
            unreachable!("graphics source");
        };
        let timeout = std::time::Duration::from_millis(timeout_ms.into());
        let Some(mut frame) = capture.next(timeout)? else {
            return Ok(None);
        };
        let copying = std::time::Instant::now();
        let (w, h) = (self.frame.width, self.frame.height);
        let rects = match frame.dirty.take() {
            Some(rects) if !self.first => rects,
            _ => vec![Rect::full(w, h)],
        };
        if rects.is_empty() {
            return Ok(None);
        }
        copy_via_staging(
            &self.context,
            staging,
            &frame.texture,
            &rects,
            &mut self.frame,
        )?;
        // Back to the pool only now: mapping the staging texture waits for
        // the copy out of the frame.
        drop(frame);
        self.last_copy = copying.elapsed();
        Ok(Some(rects))
    }

    fn next_duplicated(&mut self, timeout_ms: u32) -> Result<Option<Vec<Rect>>, CaptureError> {
        let Source::Duplication { duplication, .. } = &self.source else {
            unreachable!("duplication source");
        };
        let duplication = duplication.clone();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out-parameters owned here; the frame is released below.
        match unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(CaptureError::AccessLost),
            Err(e) => return Err(e.into()),
        }
        let copying = std::time::Instant::now();
        let result = self.process(&info, resource);
        // SAFETY: a frame was acquired above.
        match unsafe { duplication.ReleaseFrame() } {
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(CaptureError::AccessLost),
            _ => {}
        }
        // GDI reads the composed desktop, not the duplicated image: after
        // the frame is released, so Windows can get on with the next one.
        let result = match (result, &mut self.source) {
            (
                Ok(Some(rects)),
                Source::Duplication {
                    pixels: PixelSource::Gdi(reader),
                    ..
                },
            ) => {
                let (bgra, pitch) = reader.read(&rects)?;
                for r in &rects {
                    self.frame.update_from_bgra(bgra, pitch, *r);
                }
                Ok(Some(rects))
            }
            (result, _) => result,
        };
        self.last_copy = copying.elapsed();
        result
    }

    /// The regions that changed in the acquired duplication frame (`None`:
    /// nothing did). With [`PixelSource::Dxgi`] they are copied and
    /// converted here.
    fn process(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<Option<Vec<Rect>>, CaptureError> {
        // Only the mouse moved: no new desktop image.
        if info.LastPresentTime == 0 && !self.first {
            return Ok(None);
        }
        let Some(resource) = resource else {
            return Ok(None);
        };
        let (w, h) = (self.frame.width, self.frame.height);
        let rects = if self.first {
            vec![Rect::full(w, h)]
        } else {
            self.changed_rects(info.TotalMetadataBufferSize)?
        };
        if rects.is_empty() {
            return Ok(None);
        }
        let Source::Duplication {
            pixels: PixelSource::Dxgi(staging),
            ..
        } = &self.source
        else {
            return Ok(Some(rects));
        };
        let texture: ID3D11Texture2D = resource.cast()?;
        copy_via_staging(&self.context, staging, &texture, &rects, &mut self.frame)?;
        Ok(Some(rects))
    }

    /// Dirty rects plus the destinations of move rects, clipped and aligned.
    fn changed_rects(&mut self, metadata_size: u32) -> Result<Vec<Rect>, CaptureError> {
        let (w, h) = (self.frame.width, self.frame.height);
        let Source::Duplication {
            duplication,
            metadata,
            ..
        } = &mut self.source
        else {
            unreachable!("duplication source");
        };
        let to_rect = |r: RECT| Rect {
            left: r.left.max(0) as u32,
            top: r.top.max(0) as u32,
            right: r.right.max(0) as u32,
            bottom: r.bottom.max(0) as u32,
        };
        metadata.resize(metadata_size.max(1) as usize, 0);
        let mut rects = Vec::new();
        // SAFETY: the buffer is metadata_size bytes, as DXGI requires; the
        // returned sizes say how many entries were written.
        unsafe {
            let mut used = 0u32;
            duplication.GetFrameMoveRects(
                metadata.len() as u32,
                metadata.as_mut_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(),
                &mut used,
            )?;
            let moves = std::slice::from_raw_parts(
                metadata.as_ptr().cast::<DXGI_OUTDUPL_MOVE_RECT>(),
                used as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>(),
            );
            rects.extend(moves.iter().map(|m| to_rect(m.DestinationRect)));

            let mut used = 0u32;
            duplication.GetFrameDirtyRects(
                metadata.len() as u32,
                metadata.as_mut_ptr().cast::<RECT>(),
                &mut used,
            )?;
            let dirty = std::slice::from_raw_parts(
                metadata.as_ptr().cast::<RECT>(),
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

/// A CPU-readable texture of `size`.
fn staging_texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
    format: DXGI_FORMAT,
) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: a valid description; the out-parameter is owned here.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
    Ok(staging.expect("texture on success"))
}

/// Copy `rects` of `texture` into `staging`, then convert them from there
/// into `frame`.
fn copy_via_staging(
    context: &ID3D11DeviceContext,
    staging: &ID3D11Texture2D,
    texture: &ID3D11Texture2D,
    rects: &[Rect],
    frame: &mut Nv12Frame,
) -> windows::core::Result<()> {
    // SAFETY: both textures are alive and the same size; boxes are within
    // bounds (aligned); the mapping is read only while mapped.
    unsafe {
        for r in rects {
            let b = D3D11_BOX {
                left: r.left,
                top: r.top,
                front: 0,
                right: r.right,
                bottom: r.bottom,
                back: 1,
            };
            context.CopySubresourceRegion(staging, 0, r.left, r.top, 0, texture, 0, Some(&b));
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let bgra =
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * frame.height as usize);
        for r in rects {
            frame.update_from_bgra(bgra, pitch, *r);
        }
        context.Unmap(staging, 0);
    }
    Ok(())
}
