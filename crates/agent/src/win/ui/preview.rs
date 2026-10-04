//! Shows one of the agent's surfaces with made-up data, to look at it
//! (`rmm-agent ui-test <surface>`, a development aid like `capture-test`).
//! Run it in an interactive session.

use std::time::{Duration, Instant};

use anyhow::bail;
use protocol::brand::Branding;
use protocol::ipc::AgentStatus;
use tracing::info;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetSystemMetrics, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage,
    MSG, PM_REMOVE, QS_ALLINPUT, SM_CXSCREEN, SM_CYSCREEN,
};

use super::look;
use crate::win::flyout::Flyout;
use crate::win::indicator::Indicator;
use crate::win::{consent, credential};

/// Pump this thread's messages for `time`, calling `tick` each turn.
fn pump(time: Duration, mut tick: impl FnMut()) {
    let until = Instant::now() + time;
    while Instant::now() < until {
        // SAFETY: waits on, then drains, this thread's own message queue.
        unsafe {
            MsgWaitForMultipleObjects(None, false, 100, QS_ALLINPUT);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        tick();
    }
}

/// Show `surface` for `seconds`: `consent`, `password`, `about`, `bar`,
/// `pill` or `tray`. `technicians` are who is supposedly connected, and
/// `branding` the company's, if any.
pub fn run(
    surface: &str,
    technicians: &[String],
    branding: Option<Branding>,
    seconds: u64,
) -> anyhow::Result<()> {
    // SAFETY: process-wide setting, no pointers.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if let Some(branding) = &branding {
        branding.validate()?;
    }
    look::use_branding(branding);
    let time = Duration::from_secs(seconds);
    let first = technicians
        .first()
        .cloned()
        .unwrap_or_else(|| "roker".into());
    match surface {
        "consent" => {
            let _prompt = consent::show(&first, time, |answer| info!(?answer, "consent"));
            std::thread::sleep(time + Duration::from_secs(1));
        }
        "password" => {
            let prompt = credential::show(&first, |secret| {
                info!(typed = secret.is_some(), "password");
            });
            std::thread::sleep(time);
            prompt.cancel();
            std::thread::sleep(Duration::from_millis(500));
        }
        "about" => {
            super::about::show();
            std::thread::sleep(time);
        }
        "bar" | "pill" => {
            let mut bar = Indicator::new()?;
            bar.set(crate::interactive::indicator_who(technicians));
            if surface == "pill" {
                bar.collapse();
            }
            pump(time, || {
                bar.tick();
                if bar.take_end() {
                    info!("end session pressed");
                }
            });
        }
        "tray" => {
            let status = AgentStatus {
                agent_id: Some("agt-preview".into()),
                connected: true,
                version: crate::core::version().to_owned(),
            };
            let flyout = Flyout::new(status.clone())?;
            flyout.update(&status, technicians);
            // Where the tray is on a default taskbar: bottom right.
            // SAFETY: plain metric queries.
            let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            let anchor = RECT {
                left: w - 140,
                top: h - 40,
                right: w - 116,
                bottom: h - 16,
            };
            flyout.toggle(anchor);
            pump(time, || {
                for action in flyout.take_actions() {
                    info!(?action, "flyout");
                    // Picking something closes it: show it again.
                    flyout.toggle(anchor);
                }
            });
        }
        other => bail!("unknown surface {other:?}: consent, password, about, bar, pill or tray"),
    }
    Ok(())
}
