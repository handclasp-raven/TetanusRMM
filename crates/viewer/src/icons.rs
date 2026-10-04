//! The viewer's icons: the design's (`branding/viewer`), 24 units square
//! and stroked 2 wide with round caps and joins, in the form the brand's
//! own icons take so the same path code reads them.

use brand::icons::{Icon, Shape, Stroke};

/// The stroke width, in the 24-unit box.
const STROKE: f32 = 2.0;

const fn path(d: &'static str) -> Stroke {
    Stroke {
        shape: Shape::Path(d),
        width: STROKE,
    }
}

const fn rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Stroke {
    Stroke {
        shape: Shape::Rect { x, y, w, h, r },
        width: STROKE,
    }
}

const fn circle(cx: f32, cy: f32, r: f32) -> Stroke {
    Stroke {
        shape: Shape::Circle { cx, cy, r },
        width: STROKE,
    }
}

/// Ask for a whole picture again.
pub const REFRESH: Icon = Icon {
    name: "refresh",
    strokes: &[path("M20 11a8 8 0 10-2.3 5.7M20 4v7h-7")],
};

pub const FULLSCREEN: Icon = Icon {
    name: "fullscreen",
    strokes: &[path("M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5")],
};

/// A power symbol: disconnect.
pub const POWER: Icon = Icon {
    name: "power",
    strokes: &[path("M12 3v9M6.3 7a8 8 0 1011.4 0")],
};

pub const SEARCH: Icon = Icon {
    name: "search",
    strokes: &[circle(11.0, 11.0, 7.0), path("M20 20l-4-4")],
};

/// A person: a username.
pub const USER: Icon = Icon {
    name: "user",
    strokes: &[circle(12.0, 8.0, 4.0), path("M4 21a8 8 0 0116 0")],
};

/// A key: a password; the vault.
pub const KEY: Icon = Icon {
    name: "key",
    strokes: &[circle(8.0, 15.0, 4.0), path("M11 12l8-8M16 7l2 2M14 9l2 2")],
};

/// A clock: a one-time code.
pub const CLOCK: Icon = Icon {
    name: "clock",
    strokes: &[circle(12.0, 12.0, 9.0), path("M12 7v5l3 2")],
};

pub const LOCK: Icon = Icon {
    name: "lock",
    strokes: &[
        rect(5.0, 11.0, 14.0, 10.0, 2.0),
        path("M8 11V8a4 4 0 018 0v3"),
    ],
};

/// An encircled i: the agent's status.
pub const INFO: Icon = Icon {
    name: "info",
    strokes: &[circle(12.0, 12.0, 9.0), path("M12 11v5M12 8h.01")],
};

/// A prompt in a window: a command; the tools.
pub const TERMINAL: Icon = Icon {
    name: "terminal",
    strokes: &[
        rect(3.0, 4.0, 18.0, 16.0, 2.0),
        path("M7 9l3 3-3 3M13 15h4"),
    ],
};

pub const NETWORK: Icon = Icon {
    name: "network",
    strokes: &[
        circle(12.0, 5.0, 2.0),
        circle(5.0, 19.0, 2.0),
        circle(19.0, 19.0, 2.0),
        path("M12 7v5M5 17v-5h14v5"),
    ],
};

/// A monitor on its stand: remote desktop.
pub const SCREEN: Icon = Icon {
    name: "screen",
    strokes: &[rect(3.0, 4.0, 18.0, 12.0, 2.0), path("M8 20h8M12 16v4")],
};

/// A pulse: the task manager.
pub const PULSE: Icon = Icon {
    name: "pulse",
    strokes: &[path("M3 12h4l3-7 4 14 3-7h4")],
};

/// A cog: services.
pub const GEAR: Icon = Icon {
    name: "gear",
    strokes: &[
        circle(12.0, 12.0, 3.0),
        path("M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M4.9 19.1L7 17M17 7l2.1-2.1"),
    ],
};

/// A bulleted list: the event viewer.
pub const LIST: Icon = Icon {
    name: "list",
    strokes: &[path("M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01")],
};

pub const UPLOAD: Icon = Icon {
    name: "upload",
    strokes: &[path("M12 16V4M7 9l5-5 5 5M4 20h16")],
};

pub const DOWNLOAD: Icon = Icon {
    name: "download",
    strokes: &[path("M12 4v12M7 11l5 5 5-5M4 20h16")],
};

/// A check mark: the current choice of a menu.
pub const CHECK: Icon = Icon {
    name: "check",
    strokes: &[path("M5 12.5l4.5 4.5L19 7.5")],
};

pub const ALL: [Icon; 18] = [
    REFRESH, FULLSCREEN, POWER, SEARCH, USER, KEY, CLOCK, LOCK, INFO, TERMINAL, NETWORK, SCREEN,
    PULSE, GEAR, LIST, UPLOAD, DOWNLOAD, CHECK,
];

/// The icon for a command button, by the program it starts (`mstsc
/// /v:srv` is remote desktop). What is not known gets the prompt.
pub fn for_command(command: &str) -> Icon {
    let program = command.split_whitespace().next().unwrap_or_default();
    let name = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "ncpa.cpl" => NETWORK,
        "mstsc" => SCREEN,
        "taskmgr" => PULSE,
        "services.msc" => GEAR,
        "eventvwr" | "eventvwr.msc" => LIST,
        _ => TERMINAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_and_names_are_distinct() {
        let mut names = Vec::new();
        for icon in ALL {
            for (_, segs) in icon.outlines() {
                assert!(!segs.is_empty(), "{}", icon.name);
            }
            names.push(icon.name);
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
    }

    #[test]
    fn commands_get_the_icon_of_what_they_start() {
        for (command, icon) in [
            ("cmd", TERMINAL),
            ("ncpa.cpl", NETWORK),
            ("mstsc /v:srv-01", SCREEN),
            (r"C:\Windows\System32\Taskmgr.exe", PULSE),
            ("services.msc", GEAR),
            ("eventvwr.msc", LIST),
            ("notepad", TERMINAL),
            ("", TERMINAL),
        ] {
            assert_eq!(for_command(command).name, icon.name, "{command}");
        }
    }
}
