//! Gives the Windows executable its icon and version resource.

fn main() {
    brand::winres::embed(&brand::winres::Info {
        description: "TetanusRMM Quick Assist",
        file_name: "TetanusRMM-Assist.exe",
        version: env!("CARGO_PKG_VERSION"),
    });
}
