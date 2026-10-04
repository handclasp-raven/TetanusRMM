//! Draws the viewer's controls with made-up data, to look at them without
//! a session: one PPM picture per tab (and one of a menu and a dialog) in
//! the directory given.
//!
//!   cargo run -p viewer --example chrome -- OUT_DIR [TEXT_SIZE] [SCALE]
//!
//! The colours are the TUI's default theme, or the theme in
//! `RMM_VIEWER_THEME` (see `viewer::theme`).

use std::path::Path;

use protocol::media::{FrameRate, MonitorInfo};
use viewer::api::{AgentDetails, Disk};
use viewer::render::{self, Canvas, DisplayMode, Glyphs};
use viewer::theme::Theme;
use viewer::ui::{self, Chrome, Metrics, Overlay, Tab};

const SIZE: (u32, u32) = (1440, 960);

fn write_ppm(path: &Path, pixels: &[u32], (w, h): (u32, u32)) -> std::io::Result<()> {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in pixels {
        out.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
    }
    std::fs::write(path, out)
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| ".".into());
    let font_px = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(ui::DEFAULT_FONT_PX);
    let factor: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(1.0);
    let size = (
        (f64::from(SIZE.0) * factor) as u32,
        (f64::from(SIZE.1) * factor) as u32,
    );
    std::fs::create_dir_all(&dir)?;

    let theme = Theme::from_env();
    let glyphs = Glyphs::new();
    let metrics = Metrics::new(font_px, factor);
    let monitors: Vec<MonitorInfo> = (0..2)
        .map(|id| MonitorInfo {
            id,
            name: format!(r"\\.\DISPLAY{}", id + 1),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: id == 0,
        })
        .collect();
    let details = AgentDetails {
        id: "agt-1".into(),
        hostname: Some("DESKTOP-FGFU9MU".into()),
        online: true,
        os: Some("Windows 11 IoT Enterprise LTSC 2024 (build 26100)".into()),
        local_ip: Some("192.168.122.88".into()),
        remote_ip: Some("192.168.0.191".into()),
        dns_servers: Some(vec!["192.168.122.1".into()]),
        logged_in_users: Some(vec![r"DESKTOP-FGFU9MU\Claude".into()]),
        disks: Some(vec![Disk {
            name: r"C:\".into(),
            total_bytes: 199 << 30,
            used_bytes: 86 << 30,
        }]),
        capabilities: vec!["desktop".into(), "file_transfer".into()],
    };
    let commands: Vec<ui::QuickCommand> = [
        "Command prompt=cmd",
        "Network connections=ncpa.cpl",
        "Remote desktop=mstsc",
        "Task manager=taskmgr",
        "Services=services.msc",
        "Event viewer=eventvwr.msc",
    ]
    .iter()
    .map(|c| c.parse().unwrap())
    .collect();
    let locked = ui::Vault::default();
    let mut vault = ui::Vault {
        unlocked: true,
        ..Default::default()
    };
    let notices = [ui::Notice {
        text: "Starting Task manager...".into(),
        error: false,
        until: std::time::Instant::now(),
    }];

    let chrome = |tab: Option<Tab>| Chrome {
        metrics,
        layout: ui::layout(size.0, size.1, &metrics, tab.is_some()),
        theme: &theme,
        glyphs: &glyphs,
        tab,
        toolbar: ui::Toolbar {
            host: "DESKTOP-FGFU9MU",
            display: DisplayMode::Original,
            frame_rate: FrameRate::Auto,
            streamed_fps: Some(60),
            monitors: &monitors,
            active_monitor: Some(0),
            font_px,
            fullscreen: false,
        },
        footer: ui::Footer {
            link: "encrypted · direct",
            live: true,
            fps: 1,
        },
        panel: ui::Panel {
            api: true,
            details: Some(&details),
            error: None,
            commands: &commands,
            busy: None,
            lent: Some(false),
        },
        scroll: 0,
        overlay: None,
        vault: &locked,
        notices: &[],
        cursor: None,
    };
    let draw = |name: &str, chrome: Chrome| -> std::io::Result<()> {
        let mut pixels = vec![theme.backdrop; (size.0 * size.1) as usize];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            width: size.0,
            height: size.1,
        };
        render::draw_status(
            &mut canvas,
            &glyphs,
            chrome.layout.desktop,
            metrics.value(),
            theme.text,
            "Remote screen · monitor 1",
        );
        chrome.draw(&mut canvas);
        let path = Path::new(&dir).join(format!("{name}.ppm"));
        write_ppm(&path, &pixels, size)?;
        println!("{}", path.display());
        Ok(())
    };

    draw(
        "vault",
        Chrome {
            vault: &vault,
            ..chrome(Some(Tab::Vault))
        },
    )?;
    draw("vault-locked", chrome(Some(Tab::Vault)))?;
    draw("status", chrome(Some(Tab::Status)))?;
    draw(
        "tools",
        Chrome {
            notices: &notices,
            ..chrome(Some(Tab::Tools))
        },
    )?;
    draw("closed", chrome(None))?;

    // Results in the vault, a menu open, the pointer on a button.
    vault.query = "contoso".into();
    vault.focused = true;
    vault.set_rows(
        (0..6)
            .map(|i| ui::VaultRow {
                name: format!("Contoso server {i}"),
                detail: r"CORP\admin".into(),
                username: true,
                password: true,
                totp: i == 1,
            })
            .collect(),
    );
    vault.select(1);
    let base = Chrome {
        vault: &vault,
        ..chrome(Some(Tab::Vault))
    };
    let anchor = base.toolbar_buttons()[0].rect;
    let menu = Overlay::Menu(ui::display_menu(anchor, DisplayMode::Original));
    let hover = base.toolbar_buttons()[5].rect;
    draw(
        "menu",
        Chrome {
            overlay: Some(&menu),
            cursor: Some((f64::from(hover.x + 2), f64::from(hover.y + 2))),
            ..base
        },
    )?;
    let prompt = Overlay::Prompt(ui::Prompt {
        title: "Upload report.pdf to which path on the agent?".into(),
        text: r"C:\Users\Public\Desktop\report.pdf".into(),
        purpose: ui::PromptPurpose::Download,
        masked: false,
    });
    draw(
        "prompt",
        Chrome {
            overlay: Some(&prompt),
            ..base
        },
    )
}
