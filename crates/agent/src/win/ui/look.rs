//! What to draw with right now: light or dark, as Windows is set, and the
//! company's branding if the server has one.
//!
//! The branding arrives from the service (see `protocol::brand`) and is
//! kept in a small file in the user's profile, so that a helper started
//! before the service has heard from the server (or with the server out
//! of reach) already shows it. Quick assist gets it with its download.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use brand::raster::{self, Image};
use brand::{Rgb, Theme};
use protocol::brand::Branding;
use tracing::{info, warn};
use windows::core::{w, Result, PCWSTR};
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, LoadImageW, HICON, ICONINFO, IMAGE_ICON, LR_SHARED,
};

/// A company's logo, decoded.
pub struct Logo {
    pub width: u32,
    pub height: u32,
    /// Red, green, blue, alpha; alpha not premultiplied (for the tray).
    pub rgba: Vec<u8>,
    /// Blue, green, red, alpha; premultiplied (for Direct2D).
    pub pbgra: Vec<u8>,
}

struct State {
    branding: Option<Branding>,
    logo: Option<Arc<Logo>>,
}

static STATE: Mutex<State> = Mutex::new(State {
    branding: None,
    logo: None,
});

/// Counts changes of the branding, so that windows can tell when to redraw.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a `Themes\Personalize` setting says "light". Unset (Windows
/// versions without the setting) is light.
fn light_setting(value: PCWSTR) -> bool {
    let mut data = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: reads one DWORD into `data`, whose size is passed with it.
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            value,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&mut size),
        )
    };
    read.is_err() || data != 0
}

/// Whether apps (windows, dialogs) are set to dark.
pub fn apps_dark() -> bool {
    !light_setting(w!("AppsUseLightTheme"))
}

/// Whether the taskbar is dark (it has a setting of its own).
pub fn taskbar_dark() -> bool {
    !light_setting(w!("SystemUsesLightTheme"))
}

/// Everything a surface needs to draw itself now.
#[derive(Clone)]
pub struct Look {
    pub theme: Theme,
    /// The company's name, if it has branding.
    pub company: Option<String>,
    pub accent: Option<Rgb>,
    pub logo: Option<Arc<Logo>>,
}

impl Look {
    /// The look for a window that is `dark` or not.
    pub fn with(dark: bool) -> Self {
        let state = state();
        let accent = state.branding.as_ref().and_then(Branding::accent_rgb);
        Look {
            theme: Theme::new(dark, accent),
            company: state.branding.as_ref().map(|b| b.name.clone()),
            accent,
            logo: state.logo.clone(),
        }
    }

    /// The look for windows and dialogs, as Windows is set now.
    pub fn current() -> Self {
        Self::with(apps_dark())
    }

    /// What the agent is called: in the tray, About and window titles.
    pub fn agent_name(&self) -> String {
        match &self.company {
            Some(company) => format!("{company} Support Agent"),
            None => brand::AGENT_NAME.to_owned(),
        }
    }

    /// Who the technicians are, to the user: "your IT support team", or
    /// the company by name.
    pub fn team(&self) -> String {
        match &self.company {
            Some(company) => company.clone(),
            None => "your IT support team".to_owned(),
        }
    }
}

/// Changes whenever anything a window draws with does: the branding, or
/// Windows' light/dark settings.
pub fn stamp() -> (u64, bool, bool) {
    (
        GENERATION.load(Ordering::Relaxed),
        apps_dark(),
        taskbar_dark(),
    )
}

fn cache_file() -> PathBuf {
    crate::paths::helper_log().with_file_name("branding.bin")
}

/// Decode a PNG with the Windows Imaging Component.
fn decode(png: &[u8]) -> Result<Logo> {
    // SAFETY: COM calls with owned, valid arguments; the stream only reads
    // `png`, which outlives it.
    unsafe {
        // Whichever way this thread already uses COM is fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(png)?;
        let decoder = factory.CreateDecoderFromStream(
            &stream,
            std::ptr::null(),
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppRGBA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut width, mut height) = (0u32, 0u32);
        converter.GetSize(&mut width, &mut height)?;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        converter.CopyPixels(std::ptr::null(), width * 4, &mut rgba)?;
        let pbgra = rgba
            .chunks_exact(4)
            .flat_map(|p| {
                let a = u16::from(p[3]);
                let pre = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
                [pre(p[2]), pre(p[1]), pre(p[0]), p[3]]
            })
            .collect();
        Ok(Logo {
            width,
            height,
            rgba,
            pbgra,
        })
    }
}

fn apply(branding: Option<Branding>) {
    let logo = branding
        .as_ref()
        .and_then(|b| b.logo_png.as_deref())
        .and_then(|png| match decode(png) {
            Ok(logo) => Some(Arc::new(logo)),
            Err(e) => {
                warn!("the company logo cannot be decoded, the mark is used: {e}");
                None
            }
        });
    let mut state = state();
    state.branding = branding;
    state.logo = logo;
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The branding the server says to show from now on (`None`: TetanusRMM's
/// own). Remembered for the next start.
pub fn set_branding(branding: Option<Branding>) {
    let branding = branding.filter(|b| b.validate().is_ok());
    info!(?branding, "branding");
    let file = cache_file();
    let saved = match &branding {
        Some(b) => postcard::to_stdvec(b)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&file, bytes)),
        None => match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    };
    if let Err(e) = saved {
        warn!(file = %file.display(), "the branding is not remembered: {e}");
    }
    apply(branding);
}

/// Use the branding given with the program (quick assist), without
/// keeping it anywhere.
pub fn use_branding(branding: Option<Branding>) {
    apply(branding.filter(|b| b.validate().is_ok()));
}

/// Start with the branding remembered from the last run, if any.
pub fn load_cached() {
    let Ok(bytes) = std::fs::read(cache_file()) else {
        return;
    };
    match postcard::from_bytes::<Branding>(&bytes) {
        Ok(branding) if branding.validate().is_ok() => {
            info!(?branding, "remembered branding");
            apply(Some(branding));
        }
        _ => warn!("the remembered branding is unreadable and ignored"),
    }
}

/// An icon handle from a picture.
fn icon_from(image: &Image) -> Option<HICON> {
    let size = image.size as i32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `bits` points at `size * size * 4` bytes of the new bitmap,
    // which is what is written; both bitmaps are deleted once the icon
    // (which copies them) exists.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), image.rgba.len());
        for (dst, src) in pixels.chunks_exact_mut(4).zip(image.rgba.chunks_exact(4)) {
            dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
        }
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.ok()
    }
}

/// The icon for a window's title bar and the taskbar, `px` pixels square:
/// the company's logo if it has one, else the executable's own (the mark),
/// else the mark drawn here (a build without resources).
pub fn window_icon(px: i32) -> Option<HICON> {
    let look = Look::current();
    if let Some(logo) = &look.logo {
        let image = raster::fit(&logo.rgba, logo.width, logo.height, px.max(1) as u32);
        return icon_from(&image);
    }
    if look.accent.is_none() {
        // SAFETY: loads a shared icon from this module's resources.
        let own = unsafe {
            GetModuleHandleW(None).ok().and_then(|module| {
                LoadImageW(
                    Some(HINSTANCE(module.0)),
                    PCWSTR(1 as *const u16),
                    IMAGE_ICON,
                    px,
                    px,
                    LR_SHARED,
                )
                .ok()
            })
        };
        if let Some(handle) = own {
            return Some(HICON(handle.0));
        }
    }
    icon_from(&raster::app_icon(px.max(1) as u32, look.theme.accent))
}
