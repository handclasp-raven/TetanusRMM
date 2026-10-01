//! `agent capture-test`: capture and encode for a few seconds and write a raw
//! H.264 file. A development aid for checking capture and encoding on a
//! machine without the server; must run in an interactive session.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;

use super::capture::{self, Captured, ScreenCapture};
use super::encoder::H264Encoder;

pub fn run(monitor: u32, seconds: u64, out: &Path, fps: u32, bitrate: u32) -> anyhow::Result<()> {
    let monitors = capture::monitors().context("enumerating monitors")?;
    for m in &monitors {
        println!(
            "monitor {}: {} {}x{} at ({},{}){}",
            m.id,
            m.name,
            m.width,
            m.height,
            m.x,
            m.y,
            if m.primary { " primary" } else { "" }
        );
    }
    let mut dup = ScreenCapture::new(monitor).context("starting screen capture")?;
    let (w, h) = (dup.frame().width, dup.frame().height);
    let mut enc = H264Encoder::new(w, h, fps, bitrate).context("creating encoder")?;
    println!(
        "encoder: {} ({})",
        enc.info().name,
        if enc.info().hardware {
            "hardware"
        } else {
            "software"
        }
    );

    let mut file = std::fs::File::create(out)?;
    let start = Instant::now();
    let cpu_start = process_cpu_time();
    let frame_interval = Duration::from_secs(1) / fps;
    let (mut captured, mut encoded, mut keyframes, mut bytes, mut timeouts) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut changed_pixels = 0u64;
    let mut first = true;
    while start.elapsed() < Duration::from_secs(seconds) {
        let tick = Instant::now();
        match dup.next_frame(frame_interval.as_millis() as u32)? {
            Captured::Unchanged if !first => timeouts += 1,
            result => {
                if let Captured::Updated { pixels, .. } = result {
                    changed_pixels += pixels;
                }
                captured += 1;
                let pts = start.elapsed().as_micros() as u64;
                for au in enc.encode(dup.frame(), pts, first)? {
                    encoded += 1;
                    keyframes += u64::from(au.keyframe);
                    bytes += au.data.len() as u64;
                    file.write_all(&au.data)?;
                }
                first = false;
            }
        }
        if let Some(rest) = frame_interval.checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    let wall = start.elapsed().as_secs_f64();
    let cpu = process_cpu_time().saturating_sub(cpu_start).as_secs_f64();
    println!(
        "{wall:.1}s: {captured} frames captured ({timeouts} idle polls), {encoded} encoded ({keyframes} keyframes), {} KiB",
        bytes / 1024
    );
    println!(
        "changed area: {:.1} full screens; CPU: {cpu:.2}s = {:.1}% of one core",
        changed_pixels as f64 / f64::from(w * h),
        100.0 * cpu / wall
    );
    Ok(())
}

fn process_cpu_time() -> Duration {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: out-parameters for the current process.
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Duration::from_nanos((ticks(k) + ticks(u)) * 100)
}
