//! Windows Installer packages for agent download links and deployment
//! keys, built on the fly.
//!
//! Each MSI carries the published (signed) agent build, the CA certificate
//! agents must trust, and, as properties, the server address, the TLS name
//! and the link's single-use enrollment token. A deferred custom action,
//! running as LocalSystem, calls `rmm-agent.exe service install` with them;
//! the service then enrolls itself (joining the link's groups) on first start.
//! Uninstalling runs `service uninstall` before the files are removed.
//!
//! A deployment key's MSI (`reusable`) is the one to push to many machines
//! with Group Policy or Intune. It differs in two ways: a machine that is
//! already enrolled with this server keeps its identity instead of
//! enrolling again (`--keep-credential`), and its package code follows from
//! its contents, so every download of it is the same package to Windows
//! Installer.
//!
//! The package is written with the `msi` and `cab` crates, so no Windows
//! tooling is needed. It is deliberately minimal: no dialogs (msiexec shows
//! its basic progress UI), per-machine, x64 only. The product code is fixed,
//! so a second MSI on a machine that already has the agent is refused by
//! Windows Installer ("another version of this product is already
//! installed"); agents update themselves after that. Deployment tools
//! detect the agent by that product code and leave such a machine alone.
//!
//! The MSI is not Authenticode-signed (the agent binary inside it is
//! verified by its own update signature only), so Windows SmartScreen may
//! warn when it is opened from a browser download.

use std::io::{self, Cursor, Write};

use msi::{Column, Insert, Language, Package, PackageType, Value};
use protocol::brand::Branding;
use uuid::Uuid;

/// Fixed identities, so every download is "the same product".
const UPGRADE_CODE: &str = "{6D0B5C1E-3E2A-4B7B-9C55-0F1E5A2B7C41}";
const PRODUCT_CODE: &str = "{9A4C2F17-5B1D-4E63-8F0A-2C7D9E1B3A58}";
const AGENT_COMPONENT: &str = "{3F8E1A62-7C4D-4B95-A1E0-5D2B6C9F8E13}";
const CA_COMPONENT: &str = "{B27D4E90-1A3F-4C68-9E52-7F0C3D8A1B64}";

const MANUFACTURER: &str = brand::PRODUCT;
const PRODUCT_NAME: &str = brand::AGENT_NAME;

/// The icon Add/Remove Programs shows: its key in the Icon table.
const ICON: &str = "AppIcon.ico";
/// The sizes of the mark that icon carries (it is only ever shown small).
const ICON_SIZES: [u32; 4] = [16, 24, 32, 48];

/// File keys; also the file names inside the embedded cabinet.
const AGENT_FILE: &str = "rmm_agent.exe";
const CA_FILE: &str = "ca.crt";
const CABINET: &str = "agent.cab";

// Custom action type bits (msidbCustomActionType*).
const EXE_FROM_INSTALLED_FILE: i32 = 18;
const CONTINUE_ON_ERROR: i32 = 0x40;
const IN_SCRIPT: i32 = 0x400;
const NO_IMPERSONATE: i32 = 0x800;
const HIDE_TARGET: i32 = 0x2000;

/// A 64-bit component (msidbComponentAttributes64bit).
const COMPONENT_64BIT: i32 = 0x100;
/// A file stored compressed in the cabinet (msidbFileAttributesCompressed).
const FILE_COMPRESSED: i32 = 0x4000;
/// Remove on uninstall (msidbRemoveFileInstallModeOnRemove).
const ON_REMOVE: i32 = 2;

/// What goes into one package.
pub struct MsiConfig<'a> {
    /// The published agent build.
    pub agent_exe: &'a [u8],
    /// Its version (e.g. `0.1.0`), as the product version.
    pub version: &'a str,
    /// PEM CA certificate the server's certificate chains to.
    pub ca_pem: &'a str,
    /// Server QUIC address for the agent, `host:port`.
    pub server: &'a str,
    /// Name the server certificate must be valid for.
    pub server_name: &'a str,
    /// Enrollment token: a link's single-use one, or a deployment key.
    pub token: &'a str,
    /// For mass deployment with a deployment key (see the module docs).
    pub reusable: bool,
    /// The company's branding, for the name and icon in Add/Remove
    /// Programs. `None`: TetanusRMM's.
    pub branding: Option<&'a Branding>,
}

/// What Add/Remove Programs calls the product. A company's name leads if
/// there is one the package's code page can hold.
fn product_name(branding: Option<&Branding>) -> String {
    match branding {
        Some(b) if b.name.is_ascii() => format!("{} Support Agent", b.name),
        _ => PRODUCT_NAME.to_owned(),
    }
}

/// The product's icon: the company's logo if it has one that fits in an
/// icon, else the mark (on the company's colour, if it has one).
fn product_icon(branding: Option<&Branding>) -> Vec<u8> {
    let logo = branding.and_then(|b| b.logo_png.as_deref());
    if let Some(ico) = logo.and_then(brand::ico::from_png) {
        return ico;
    }
    let tile = branding
        .and_then(Branding::accent_rgb)
        .unwrap_or(brand::theme::palette::RUST);
    let images: Vec<_> = ICON_SIZES
        .into_iter()
        .map(|size| brand::raster::app_icon(size, tile))
        .collect();
    brand::ico::encode(&images)
}

/// Whether `value` is a plain host name or IP address (it ends up on a
/// command line, so nothing else is allowed).
pub fn valid_host(value: &str) -> bool {
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        return inner.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !value.is_empty()
        && value.len() <= protocol::MAX_HOSTNAME_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !value.starts_with(['-', '.'])
}

/// Whether `value` is `host:port` (IPv6 in brackets) with a non-zero port.
pub fn valid_host_port(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    matches!(port.parse::<u16>(), Ok(p) if p != 0) && valid_host(host)
}

/// `major.minor.build` for Windows Installer (numeric parts only).
fn product_version(version: &str) -> String {
    let core = version.split(['-', '+']).next().unwrap_or("0");
    let parts: Vec<u32> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let part = |i: usize| parts.get(i).copied().unwrap_or(0);
    format!(
        "{}.{}.{}",
        part(0).min(255),
        part(1).min(255),
        part(2).min(65535)
    )
}

/// A package code that follows from what goes into the package.
fn package_code(config: &MsiConfig) -> Uuid {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for part in [
        config.agent_exe,
        config.ca_pem.as_bytes(),
        config.server.as_bytes(),
        config.server_name.as_bytes(),
        config.token.as_bytes(),
        product_name(config.branding).as_bytes(),
        &product_icon(config.branding),
    ] {
        context.update(&(part.len() as u64).to_be_bytes());
        context.update(part);
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&context.finish().as_ref()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

/// Build the MSI.
pub fn build(config: &MsiConfig) -> io::Result<Vec<u8>> {
    if !valid_host_port(config.server) {
        return Err(invalid("server must be host:port"));
    }
    if !valid_host(config.server_name) {
        return Err(invalid("server name must be a host name or IP address"));
    }
    if config.token.is_empty() || !config.token.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(invalid("malformed enrollment token"));
    }
    let cabinet = cabinet(&[
        (AGENT_FILE, config.agent_exe),
        (CA_FILE, config.ca_pem.as_bytes()),
    ])?;

    let mut package = Package::create(PackageType::Installer, Cursor::new(Vec::new()))?;
    package.set_database_codepage(msi::CodePage::Windows1252);
    let summary = package.summary_info_mut();
    summary.set_codepage(msi::CodePage::Windows1252);
    summary.set_title("Installation Database");
    summary.set_subject(PRODUCT_NAME);
    summary.set_author(MANUFACTURER);
    summary.set_comments("Installs the RMM agent service and enrolls it with the server.");
    summary.set_arch("x64");
    summary.set_languages(&[Language::from_code(1033)]);
    // Package code: unique per package.
    summary.set_uuid(if config.reusable {
        package_code(config)
    } else {
        Uuid::new_v4()
    });
    summary.set_page_count(500); // Windows Installer 5.0
    summary.set_word_count(2); // long file names, compressed source
    summary.set_doc_security(2);
    summary.set_creating_application("rmm server");
    summary.set_creation_time_to_now();

    let version = product_version(config.version);
    let product_name = product_name(config.branding);
    let s = |v: &str| Value::from(v);
    let null = || Value::Null;

    table(
        &mut package,
        "Property",
        vec![
            Column::build("Property").primary_key().id_string(72),
            Column::build("Value").localizable().text_string(0),
        ],
        [
            ("ProductCode", PRODUCT_CODE),
            ("UpgradeCode", UPGRADE_CODE),
            ("ProductName", product_name.as_str()),
            ("ProductVersion", version.as_str()),
            ("ProductLanguage", "1033"),
            ("Manufacturer", MANUFACTURER),
            ("ALLUSERS", "1"),
            ("ARPNOMODIFY", "1"),
            ("ARPNOREPAIR", "1"),
            ("ARPPRODUCTICON", ICON),
            ("RMM_SERVER", config.server),
            ("RMM_SERVER_NAME", config.server_name),
            ("RMM_TOKEN", config.token),
            ("MsiHiddenProperties", "RMM_TOKEN"),
            (
                "SecureCustomProperties",
                "RMM_SERVER;RMM_SERVER_NAME;RMM_TOKEN",
            ),
        ]
        .into_iter()
        .map(|(k, v)| vec![s(k), s(v)])
        .collect(),
    )?;

    table(
        &mut package,
        "Directory",
        vec![
            Column::build("Directory").primary_key().id_string(72),
            Column::build("Directory_Parent").nullable().id_string(72),
            Column::build("DefaultDir").localizable().string(255),
        ],
        vec![
            vec![s("TARGETDIR"), null(), s("SourceDir")],
            vec![s("ProgramFiles64Folder"), s("TARGETDIR"), s(".")],
            vec![s("INSTALLDIR"), s("ProgramFiles64Folder"), s("RMM")],
        ],
    )?;

    table(
        &mut package,
        "Component",
        vec![
            Column::build("Component").primary_key().id_string(72),
            Column::build("ComponentId").nullable().string(38),
            Column::build("Directory_").id_string(72),
            Column::build("Attributes").int16(),
            Column::build("Condition").nullable().string(255),
            Column::build("KeyPath").nullable().id_string(72),
        ],
        vec![
            vec![
                s("Agent"),
                s(AGENT_COMPONENT),
                s("INSTALLDIR"),
                Value::from(COMPONENT_64BIT),
                null(),
                s(AGENT_FILE),
            ],
            vec![
                s("ServerCa"),
                s(CA_COMPONENT),
                s("INSTALLDIR"),
                Value::from(COMPONENT_64BIT),
                null(),
                s(CA_FILE),
            ],
        ],
    )?;

    table(
        &mut package,
        "Feature",
        vec![
            Column::build("Feature").primary_key().id_string(38),
            Column::build("Feature_Parent").nullable().id_string(38),
            Column::build("Title").nullable().localizable().string(64),
            Column::build("Description")
                .nullable()
                .localizable()
                .string(255),
            Column::build("Display").nullable().int16(),
            Column::build("Level").int16(),
            Column::build("Directory_").nullable().id_string(72),
            Column::build("Attributes").int16(),
        ],
        vec![vec![
            s("Main"),
            null(),
            s(PRODUCT_NAME),
            s("The agent service."),
            Value::from(1),
            Value::from(1),
            s("INSTALLDIR"),
            Value::from(0),
        ]],
    )?;

    table(
        &mut package,
        "FeatureComponents",
        vec![
            Column::build("Feature_").primary_key().id_string(38),
            Column::build("Component_").primary_key().id_string(72),
        ],
        vec![vec![s("Main"), s("Agent")], vec![s("Main"), s("ServerCa")]],
    )?;

    let size = |b: &[u8]| i32::try_from(b.len()).map_err(|_| invalid("file too large"));
    table(
        &mut package,
        "File",
        vec![
            Column::build("File").primary_key().id_string(72),
            Column::build("Component_").id_string(72),
            Column::build("FileName").localizable().string(255),
            Column::build("FileSize").int32(),
            Column::build("Version").nullable().string(72),
            Column::build("Language").nullable().string(20),
            Column::build("Attributes").nullable().int16(),
            Column::build("Sequence").int32(),
        ],
        vec![
            vec![
                s(AGENT_FILE),
                s("Agent"),
                s("RMM-AG~1.EXE|rmm-agent.exe"),
                Value::from(size(config.agent_exe)?),
                null(),
                null(),
                Value::from(FILE_COMPRESSED),
                Value::from(1),
            ],
            vec![
                s(CA_FILE),
                s("ServerCa"),
                s("ca.crt"),
                Value::from(size(config.ca_pem.as_bytes())?),
                null(),
                null(),
                Value::from(FILE_COMPRESSED),
                Value::from(2),
            ],
        ],
    )?;

    // Leftovers of the agent's self-updates (`rmm-agent.old-*.exe`).
    table(
        &mut package,
        "RemoveFile",
        vec![
            Column::build("FileKey").primary_key().id_string(72),
            Column::build("Component_").id_string(72),
            Column::build("FileName")
                .nullable()
                .localizable()
                .string(255),
            Column::build("DirProperty").id_string(72),
            Column::build("InstallMode").int16(),
        ],
        vec![vec![
            s("OldAgents"),
            s("Agent"),
            s("*.old*"),
            s("INSTALLDIR"),
            Value::from(ON_REMOVE),
        ]],
    )?;

    table(
        &mut package,
        "Media",
        vec![
            Column::build("DiskId").primary_key().int16(),
            Column::build("LastSequence").int32(),
            Column::build("DiskPrompt")
                .nullable()
                .localizable()
                .string(64),
            Column::build("Cabinet").nullable().string(255),
            Column::build("VolumeLabel").nullable().string(32),
            Column::build("Source").nullable().string(72),
        ],
        vec![vec![
            Value::from(1),
            Value::from(2),
            null(),
            s(&format!("#{CABINET}")),
            null(),
            null(),
        ]],
    )?;

    let in_script = EXE_FROM_INSTALLED_FILE | IN_SCRIPT | NO_IMPERSONATE;
    let keep_credential = if config.reusable {
        " --keep-credential"
    } else {
        ""
    };
    table(
        &mut package,
        "CustomAction",
        vec![
            Column::build("Action").primary_key().id_string(72),
            Column::build("Type").int16(),
            Column::build("Source").nullable().string(72),
            Column::build("Target").nullable().string(255),
        ],
        vec![
            vec![
                s("RmmInstallService"),
                Value::from(in_script | HIDE_TARGET),
                s(AGENT_FILE),
                s(&format!(
                    "service install --server [RMM_SERVER] --server-name [RMM_SERVER_NAME] \
                     --server-ca \"[#{CA_FILE}]\" --token [RMM_TOKEN]{keep_credential}"
                )),
            ],
            // A service removed by hand is not a reason to fail.
            vec![
                s("RmmUninstallService"),
                Value::from(in_script | CONTINUE_ON_ERROR),
                s(AGENT_FILE),
                s("service uninstall"),
            ],
        ],
    )?;

    let sequence_columns = || {
        vec![
            Column::build("Action").primary_key().id_string(72),
            Column::build("Condition").nullable().string(255),
            Column::build("Sequence").nullable().int16(),
        ]
    };
    let step = |action: &str, condition: Option<&str>, seq: i32| {
        vec![
            s(action),
            condition.map_or(Value::Null, s),
            Value::from(seq),
        ]
    };
    table(
        &mut package,
        "InstallExecuteSequence",
        sequence_columns(),
        vec![
            step("CostInitialize", None, 800),
            step("FileCost", None, 900),
            step("CostFinalize", None, 1000),
            step("InstallValidate", None, 1400),
            step("InstallInitialize", None, 1500),
            step("ProcessComponents", None, 1600),
            step("UnpublishFeatures", None, 1800),
            step("RmmUninstallService", Some("REMOVE~=\"ALL\""), 3400),
            step("RemoveFiles", None, 3500),
            step("InstallFiles", None, 4000),
            step(
                "RmmInstallService",
                Some("NOT Installed AND NOT REMOVE"),
                6000,
            ),
            step("RegisterProduct", None, 6100),
            step("PublishFeatures", None, 6300),
            step("PublishProduct", None, 6400),
            step("InstallFinalize", None, 6600),
        ],
    )?;
    table(
        &mut package,
        "InstallUISequence",
        sequence_columns(),
        vec![
            step("CostInitialize", None, 800),
            step("FileCost", None, 900),
            step("CostFinalize", None, 1000),
            step("ExecuteAction", None, 1300),
        ],
    )?;
    // Group Policy advertises a package before it installs it.
    table(
        &mut package,
        "AdvtExecuteSequence",
        sequence_columns(),
        vec![
            step("CostInitialize", None, 800),
            step("CostFinalize", None, 1000),
            step("InstallValidate", None, 1400),
            step("InstallInitialize", None, 1500),
            step("PublishFeatures", None, 6300),
            step("PublishProduct", None, 6400),
            step("InstallFinalize", None, 6600),
        ],
    )?;
    table(
        &mut package,
        "AdminExecuteSequence",
        sequence_columns(),
        vec![
            step("CostInitialize", None, 800),
            step("FileCost", None, 900),
            step("CostFinalize", None, 1000),
            step("InstallValidate", None, 1400),
            step("InstallInitialize", None, 1500),
            step("InstallAdminPackage", None, 3900),
            step("InstallFiles", None, 4000),
            step("InstallFinalize", None, 6600),
        ],
    )?;
    table(
        &mut package,
        "AdminUISequence",
        sequence_columns(),
        vec![
            step("CostInitialize", None, 800),
            step("FileCost", None, 900),
            step("CostFinalize", None, 1000),
            step("ExecuteAction", None, 1300),
        ],
    )?;

    // The icon's bytes are the stream named after its row.
    table(
        &mut package,
        "Icon",
        vec![
            Column::build("Name").primary_key().id_string(72),
            Column::build("Data").binary(),
        ],
        vec![vec![s(ICON), Value::Binary]],
    )?;
    package
        .write_stream(&format!("Icon.{ICON}"))?
        .write_all(&product_icon(config.branding))?;

    package.write_stream(CABINET)?.write_all(&cabinet)?;
    package.flush()?;
    Ok(package.into_inner()?.into_inner())
}

fn table(
    package: &mut Package<Cursor<Vec<u8>>>,
    name: &str,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
) -> io::Result<()> {
    package.create_table(name, columns)?;
    package.insert_rows(Insert::into(name).rows(rows))
}

/// An MSZIP cabinet holding `files`, in order (their File-table sequence).
fn cabinet(files: &[(&str, &[u8])]) -> io::Result<Vec<u8>> {
    let mut builder = cab::CabinetBuilder::new();
    let folder = builder.add_folder(cab::CompressionType::MsZip);
    for (name, _) in files {
        folder.add_file(*name);
    }
    let mut writer = builder.build(Cursor::new(Vec::new()))?;
    let mut contents = files.iter();
    while let Some(mut file) = writer.next_file()? {
        let (_, data) = contents.next().expect("one entry per file");
        file.write_all(data)?;
    }
    Ok(writer.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use msi::Select;
    use std::io::Read;

    fn config<'a>(exe: &'a [u8]) -> MsiConfig<'a> {
        MsiConfig {
            agent_exe: exe,
            version: "0.3.1-beta.2",
            ca_pem: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
            server: "rmm.example.com:4433",
            server_name: "rmm.example.com",
            token: "0123abcdef",
            reusable: false,
            branding: None,
        }
    }

    fn install_command(package: &mut Package<Cursor<Vec<u8>>>) -> String {
        let rows = package.select_rows(Select::table("CustomAction")).unwrap();
        for row in rows {
            if row[0].as_str() == Some("RmmInstallService") {
                return row[3].as_str().unwrap().to_owned();
            }
        }
        panic!("no install action");
    }

    fn property(package: &mut Package<Cursor<Vec<u8>>>, name: &str) -> String {
        let rows = package.select_rows(Select::table("Property")).unwrap();
        for row in rows {
            if row[0].as_str() == Some(name) {
                return row[1].as_str().unwrap().to_owned();
            }
        }
        panic!("no property {name}");
    }

    fn icon(package: &mut Package<Cursor<Vec<u8>>>) -> Vec<u8> {
        let rows: Vec<String> = package
            .select_rows(Select::table("Icon"))
            .unwrap()
            .map(|row| row[0].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(rows, [ICON]);
        let mut bytes = Vec::new();
        package
            .read_stream(&format!("Icon.{ICON}"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    }

    #[test]
    fn add_remove_programs_shows_the_mark_or_the_companys_brand() {
        let exe = [0x4d, 0x5a, 1, 2, 3].repeat(100);
        let plain = build(&config(&exe)).unwrap();
        let mut package = Package::open(Cursor::new(plain)).unwrap();
        assert_eq!(property(&mut package, "ProductName"), "TetanusRMM Agent");
        assert_eq!(property(&mut package, "Manufacturer"), "TetanusRMM");
        assert_eq!(property(&mut package, "ARPPRODUCTICON"), ICON);
        let mark = icon(&mut package);
        // An icon file with the four small sizes of the mark.
        assert_eq!(mark[..6], [0, 0, 1, 0, 4, 0]);
        assert_eq!([mark[6], mark[22], mark[38], mark[54]], [16, 24, 32, 48]);

        // A company's name leads, and its logo is the icon as it is.
        let mut logo = b"\x89PNG\r\n\x1a\n".to_vec();
        logo.extend_from_slice(&13u32.to_be_bytes());
        logo.extend_from_slice(b"IHDR");
        logo.extend_from_slice(&64u32.to_be_bytes());
        logo.extend_from_slice(&64u32.to_be_bytes());
        logo.extend_from_slice(&[8, 6, 0, 0, 0, 1, 2, 3, 4]);
        let mut contoso = Branding {
            name: "Contoso IT".into(),
            accent: Some([0x0B, 0x5C, 0xAD]),
            logo_png: Some(logo.clone()),
        };
        let branded = |b: &Branding| {
            let bytes = build(&MsiConfig {
                branding: Some(b),
                reusable: true,
                ..config(&exe)
            })
            .unwrap();
            Package::open(Cursor::new(bytes)).unwrap()
        };
        let mut package = branded(&contoso);
        assert_eq!(
            property(&mut package, "ProductName"),
            "Contoso IT Support Agent"
        );
        let with_logo = icon(&mut package);
        assert!(with_logo.ends_with(&logo) && with_logo.len() == 22 + logo.len());
        let code = package.summary_info().uuid();

        // No logo: the mark, on the company's colour. A name the package
        // cannot hold leaves the product's own. Either changes the package
        // code, since the package differs.
        contoso.logo_png = None;
        contoso.name = "Contos\u{14d} IT".into();
        let mut package = branded(&contoso);
        assert_eq!(property(&mut package, "ProductName"), "TetanusRMM Agent");
        let tinted = icon(&mut package);
        assert_eq!(tinted.len(), mark.len());
        assert_ne!(tinted, mark);
        assert_ne!(package.summary_info().uuid(), code);
    }

    #[test]
    fn package_carries_the_agent_ca_and_install_settings() {
        let exe = [0x4d, 0x5a, 1, 2, 3].repeat(10_000);
        let bytes = build(&config(&exe)).unwrap();
        let mut package = Package::open(Cursor::new(bytes)).unwrap();
        assert_eq!(package.package_type(), PackageType::Installer);
        assert_eq!(package.summary_info().arch(), Some("x64"));
        assert_eq!(property(&mut package, "ProductVersion"), "0.3.1");
        assert_eq!(property(&mut package, "RMM_SERVER"), "rmm.example.com:4433");
        assert_eq!(property(&mut package, "RMM_SERVER_NAME"), "rmm.example.com");
        assert_eq!(property(&mut package, "RMM_TOKEN"), "0123abcdef");

        // The embedded cabinet holds both files, byte for byte.
        let mut cab_bytes = Vec::new();
        package
            .read_stream(CABINET)
            .unwrap()
            .read_to_end(&mut cab_bytes)
            .unwrap();
        let mut cabinet = cab::Cabinet::new(Cursor::new(cab_bytes)).unwrap();
        let mut agent = Vec::new();
        cabinet
            .read_file(AGENT_FILE)
            .unwrap()
            .read_to_end(&mut agent)
            .unwrap();
        assert_eq!(agent, exe);
        let mut ca = String::new();
        cabinet
            .read_file(CA_FILE)
            .unwrap()
            .read_to_string(&mut ca)
            .unwrap();
        assert!(ca.starts_with("-----BEGIN CERTIFICATE-----"));

        // Fresh package code each time; same product.
        let other = build(&config(&exe)).unwrap();
        let other = Package::open(Cursor::new(other)).unwrap();
        assert_ne!(other.summary_info().uuid(), package.summary_info().uuid());

        assert!(install_command(&mut package).ends_with("--token [RMM_TOKEN]"));
        // Group Policy can advertise it.
        for sequence in ["AdvtExecuteSequence", "AdminExecuteSequence"] {
            assert!(package.has_table(sequence), "{sequence}");
        }
    }

    #[test]
    fn a_reusable_package_keeps_an_enrolled_machine_and_is_the_same_each_time() {
        let exe = [0x4d, 0x5a, 1, 2, 3].repeat(100);
        let mut config = config(&exe);
        config.reusable = true;
        let open = |config: &MsiConfig| Package::open(Cursor::new(build(config).unwrap())).unwrap();
        let mut package = open(&config);
        assert!(install_command(&mut package).ends_with("--token [RMM_TOKEN] --keep-credential"));

        // Same contents, same package code; another key, another package.
        let code = package.summary_info().uuid();
        assert!(code.is_some());
        assert_eq!(open(&config).summary_info().uuid(), code);
        config.token = "fedcba3210";
        assert_ne!(open(&config).summary_info().uuid(), code);
    }

    #[test]
    fn settings_that_would_break_the_command_line_are_refused() {
        let exe = [0u8; 4];
        let mut bad = config(&exe);
        bad.server = "rmm.example.com:4433 --token x";
        assert!(build(&bad).is_err());
        let mut bad = config(&exe);
        bad.server_name = "a\"b";
        assert!(build(&bad).is_err());
        let mut bad = config(&exe);
        bad.token = "x y";
        assert!(build(&bad).is_err());
    }

    #[test]
    fn host_and_port_checks() {
        for ok in [
            "rmm.example.com:4433",
            "192.0.2.1:1",
            "[::1]:4433",
            "localhost:65535",
        ] {
            assert!(valid_host_port(ok), "{ok}");
        }
        for bad in [
            "rmm",
            "rmm:0",
            "rmm:70000",
            ":4433",
            "-x:1",
            "a b:1",
            "[zz]:1",
            "::1:4433",
        ] {
            assert!(!valid_host_port(bad), "{bad}");
        }
        assert!(valid_host("192.168.122.1") && valid_host("[fd00::1]"));
        assert_eq!(product_version("1.2"), "1.2.0");
    }
}
