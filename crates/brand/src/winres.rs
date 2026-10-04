//! For build scripts: give a Windows executable the application icon and
//! a version resource (what Explorer shows as its description).
//!
//! The icon is drawn here, so no binary asset is kept in the repository.
//! The resource is compiled with `llvm-rc` (cross-compiling, as the
//! release builds do) or `rc` (a Windows SDK on `PATH`). With neither, the
//! build goes on without an icon and says so.

use std::path::Path;
use std::process::Command;

use crate::theme::palette;
use crate::{ico, raster};

/// What the version resource says.
pub struct Info<'a> {
    /// `FileDescription`: what Task Manager and Explorer call the program.
    pub description: &'a str,
    /// The file's name, e.g. `rmm-agent.exe`.
    pub file_name: &'a str,
    /// `major.minor.patch`.
    pub version: &'a str,
}

/// `1.2.3` (or `1.2.3-rc1`) as the four numbers a version resource takes.
fn version_numbers(version: &str) -> [u16; 4] {
    let mut out = [0u16; 4];
    let core = version.split(['-', '+']).next().unwrap_or_default();
    for (slot, part) in out.iter_mut().zip(core.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    out
}

/// The resource script: the icon as resource 1 (the one Explorer shows),
/// and the version information.
fn script(icon: &Path, info: &Info) -> String {
    let [a, b, c, d] = version_numbers(info.version);
    // Backslashes in a quoted path would be escapes.
    let icon = icon.display().to_string().replace('\\', "/");
    let quote = |s: &str| s.replace('"', "'");
    format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {a},{b},{c},{d}
PRODUCTVERSION {a},{b},{c},{d}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "{product}"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{file}"
      VALUE "OriginalFilename", "{file}"
      VALUE "ProductName", "{product}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        product = crate::PRODUCT,
        description = quote(info.description),
        version = quote(info.version),
        file = quote(info.file_name),
    )
}

/// Call from `build.rs`. Does nothing unless the target is Windows.
pub fn embed(info: &Info) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR for build scripts");
    let out = Path::new(&out);
    let icon = out.join("app.ico");
    let rc = out.join("app.rc");
    let res = out.join("app.res");
    let written = std::fs::write(&icon, ico::encode(&raster::app_icons(palette::RUST)))
        .and_then(|()| std::fs::write(&rc, script(&icon, info)));
    if let Err(e) = written {
        println!("cargo:warning=no icon: cannot write the resource script: {e}");
        return;
    }
    let compiled = ["llvm-rc", "rc"].into_iter().any(|compiler| {
        // Both take `/fo <output> <script>`.
        Command::new(compiler)
            .arg("/fo")
            .arg(&res)
            .arg(&rc)
            .output()
            .is_ok_and(|done| done.status.success() && res.is_file())
    });
    if compiled {
        // The MSVC linker (and lld-link) take a compiled resource as an input.
        println!("cargo:rustc-link-arg-bins={}", res.display());
    } else {
        println!(
            "cargo:warning=no icon: neither llvm-rc nor rc could compile the resources \
             (put one on PATH)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_become_four_numbers() {
        assert_eq!(version_numbers("0.1.7"), [0, 1, 7, 0]);
        assert_eq!(version_numbers("1.2.3-rc1"), [1, 2, 3, 0]);
        assert_eq!(version_numbers("2.0"), [2, 0, 0, 0]);
        assert_eq!(version_numbers("nonsense"), [0, 0, 0, 0]);
    }

    #[test]
    fn the_script_names_the_icon_and_the_program() {
        let text = script(
            Path::new(r"C:\out\app.ico"),
            &Info {
                description: "TetanusRMM Agent",
                file_name: "rmm-agent.exe",
                version: "0.1.7",
            },
        );
        assert!(text.starts_with("1 ICON \"C:/out/app.ico\""));
        assert!(text.contains("FILEVERSION 0,1,7,0"));
        assert!(text.contains("VALUE \"FileDescription\", \"TetanusRMM Agent\""));
        assert!(text.contains("VALUE \"ProductName\", \"TetanusRMM\""));
    }
}
