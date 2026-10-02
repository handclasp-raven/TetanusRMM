//! Signing and publishing agent updates.
//!
//! Dev workflow:
//!
//! 1. `server gen-update-key` makes an ed25519 key pair. The public key is
//!    baked into agent builds via the `RMM_UPDATE_PUBKEY` env var.
//! 2. `server sign-update <file> --platform P --version V` writes `<file>.sig`,
//!    a detached signature over [`protocol::update::signed_message`].
//! 3. `server publish-update <file> --platform P --version V` copies the binary
//!    and its signature into the updates directory and writes the manifest the
//!    API serves.
//!
//! Layout: `<updates_dir>/<platform>/{manifest.json, agent, agent.sig}`.
//!
//! Agents built without `RMM_UPDATE_PUBKEY` (the released builds, which are
//! the same for every server) fetch the public key from the API once and pin
//! it. `publish_public_key` puts it at `<updates_dir>/update.pub` for that.
//!
//! Viewer builds are published next to them (`publish_viewer`), as
//! `<updates_dir>/<platform>/{viewer.json, viewer}`, for the support TUI to
//! download. They are not signed with the update key: a signature made for a
//! viewer would also pass an agent's update check.
//!
//! The quick assist client (`publish_assist`) sits there too, as
//! `<updates_dir>/<platform>/{assist.json, assist}`; the download page
//! appends each server's settings to it (see `protocol::assist`).
//!
//! The support TUI's wheel is published as `<updates_dir>/tui/<wheel>`
//! (`publish_tui`), under its own file name, for the install page.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use protocol::update::{signed_message, valid_platform, UpdateManifest};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const BINARY_FILE: &str = "agent";
pub const SIGNATURE_FILE: &str = "agent.sig";
pub const VIEWER_MANIFEST_FILE: &str = "viewer.json";
pub const VIEWER_BINARY_FILE: &str = "viewer";
pub const ASSIST_MANIFEST_FILE: &str = "assist.json";
pub const ASSIST_BINARY_FILE: &str = "assist";
/// Directory of the published TUI wheel, beside the platform directories.
pub const TUI_DIR: &str = "tui";
pub const SIGNING_KEY_FILE: &str = "update.key";
pub const PUBLIC_KEY_FILE: &str = "update.pub";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid platform name {0:?}")]
    InvalidPlatform(String),
    #[error("invalid version {0:?}: must be semver like 1.2.3")]
    InvalidVersion(String),
    #[error("{0:?} is not a TUI wheel: expected a file named tetanus_rmm-<version>-….whl")]
    InvalidWheel(String),
    #[error("invalid key file: expected 64 hex characters")]
    InvalidKey,
    #[error("invalid signature file: expected 64 bytes")]
    InvalidSignature,
    #[error("malformed manifest: {0}")]
    Manifest(#[from] serde_json::Error),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> UpdateError + '_ {
    move |source| UpdateError::Io {
        path: path.to_owned(),
        source,
    }
}

fn check_release(platform: &str, version: &str) -> Result<(), UpdateError> {
    if !valid_platform(platform) {
        return Err(UpdateError::InvalidPlatform(platform.to_owned()));
    }
    semver::Version::parse(version).map_err(|_| UpdateError::InvalidVersion(version.to_owned()))?;
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(digest(&SHA256, bytes))
}

/// A new random signing key.
pub fn generate_key() -> SigningKey {
    let mut seed = [0u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .expect("system RNG available");
    SigningKey::from_bytes(&seed)
}

/// Write `update.key` (hex seed, mode 0600) and `update.pub` (hex) into `dir`.
pub fn write_key_pair(dir: &Path, key: &SigningKey) -> Result<(), UpdateError> {
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    let secret = dir.join(SIGNING_KEY_FILE);
    common::fs::write_file(&secret, hex::encode(key.to_bytes()).as_bytes(), true)
        .map_err(io_err(&secret))?;
    let public = dir.join(PUBLIC_KEY_FILE);
    common::fs::write_file(
        &public,
        public_key_hex(&key.verifying_key()).as_bytes(),
        false,
    )
    .map_err(io_err(&public))
}

pub fn public_key_hex(key: &VerifyingKey) -> String {
    hex::encode(key.to_bytes())
}

pub fn load_signing_key(path: &Path) -> Result<SigningKey, UpdateError> {
    let text = fs::read_to_string(path).map_err(io_err(path))?;
    let seed: [u8; 32] = hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(UpdateError::InvalidKey)?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Sign a release of `binary` for `platform` at `version`.
pub fn sign(
    key: &SigningKey,
    platform: &str,
    version: &str,
    binary: &[u8],
) -> Result<Signature, UpdateError> {
    check_release(platform, version)?;
    Ok(key.sign(&signed_message(platform, version, &sha256_hex(binary))))
}

/// Path of the detached signature for `file`: `<file>.sig`.
pub fn signature_path_for(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".sig");
    PathBuf::from(name)
}

/// Sign `file` with the key at `key_path`, writing `<file>.sig` (64 raw bytes).
pub fn sign_file(
    key_path: &Path,
    file: &Path,
    platform: &str,
    version: &str,
) -> Result<PathBuf, UpdateError> {
    let key = load_signing_key(key_path)?;
    let binary = fs::read(file).map_err(io_err(file))?;
    let signature = sign(&key, platform, version, &binary)?;
    let out = signature_path_for(file);
    fs::write(&out, signature.to_bytes()).map_err(io_err(&out))?;
    Ok(out)
}

/// Copy `file` and `<file>.sig` into `<updates_dir>/<platform>/` and write
/// the manifest. The manifest is written last, so a client never sees a
/// manifest for files that are not there yet. (A client racing a publish may
/// see a mismatched binary/signature pair; that fails verification and the
/// agent retries later.)
pub fn publish(
    updates_dir: &Path,
    file: &Path,
    platform: &str,
    version: &str,
) -> Result<UpdateManifest, UpdateError> {
    check_release(platform, version)?;
    let binary = fs::read(file).map_err(io_err(file))?;
    let sig_path = signature_path_for(file);
    let signature = fs::read(&sig_path).map_err(io_err(&sig_path))?;
    if signature.len() != Signature::BYTE_SIZE {
        return Err(UpdateError::InvalidSignature);
    }

    let manifest = UpdateManifest {
        platform: platform.to_owned(),
        version: version.to_owned(),
        sha256: sha256_hex(&binary),
        size: binary.len() as u64,
    };
    write_release(
        &updates_dir.join(platform),
        &[
            (BINARY_FILE, binary.as_slice()),
            (SIGNATURE_FILE, signature.as_slice()),
            (
                MANIFEST_FILE,
                serde_json::to_vec_pretty(&manifest)?.as_slice(),
            ),
        ],
    )?;
    Ok(manifest)
}

/// Copy the public key file `key_file` (`update.pub`) into `updates_dir`,
/// where the API serves it to agents that have no key baked in.
pub fn publish_public_key(updates_dir: &Path, key_file: &Path) -> Result<(), UpdateError> {
    let text = fs::read_to_string(key_file).map_err(io_err(key_file))?;
    let key = parse_public_key(&text).ok_or(UpdateError::InvalidKey)?;
    write_release(
        updates_dir,
        &[(PUBLIC_KEY_FILE, public_key_hex(&key).as_bytes())],
    )
}

/// The published public key (hex), if any.
pub fn load_public_key(updates_dir: &Path) -> Result<Option<String>, UpdateError> {
    let path = updates_dir.join(PUBLIC_KEY_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => parse_public_key(&text)
            .map(|key| Some(public_key_hex(&key)))
            .ok_or(UpdateError::InvalidKey),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&path)(e)),
    }
}

fn parse_public_key(text: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(text.trim()).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// Write `files` into `dir` in order, each through a temporary file.
fn write_release(dir: &Path, files: &[(&str, &[u8])]) -> Result<(), UpdateError> {
    fs::create_dir_all(dir).map_err(io_err(dir))?;
    for (name, bytes) in files {
        let tmp = dir.join(format!(".{name}.tmp"));
        let dest = dir.join(name);
        fs::write(&tmp, bytes).map_err(io_err(&tmp))?;
        fs::rename(&tmp, &dest).map_err(io_err(&dest))?;
    }
    Ok(())
}

/// Copy the viewer build `file` into `<updates_dir>/<platform>/` and write
/// its manifest (last, as in [`publish`]). The support TUI downloads it over
/// HTTPS and checks it against the manifest's SHA-256.
pub fn publish_viewer(
    updates_dir: &Path,
    file: &Path,
    platform: &str,
    version: &str,
) -> Result<UpdateManifest, UpdateError> {
    publish_unsigned(
        updates_dir,
        file,
        platform,
        version,
        VIEWER_BINARY_FILE,
        VIEWER_MANIFEST_FILE,
    )
}

/// Copy the quick assist build `file` into `<updates_dir>/<platform>/` and
/// write its manifest, for the `/assist` page to offer.
pub fn publish_assist(
    updates_dir: &Path,
    file: &Path,
    platform: &str,
    version: &str,
) -> Result<UpdateManifest, UpdateError> {
    publish_unsigned(
        updates_dir,
        file,
        platform,
        version,
        ASSIST_BINARY_FILE,
        ASSIST_MANIFEST_FILE,
    )
}

fn publish_unsigned(
    updates_dir: &Path,
    file: &Path,
    platform: &str,
    version: &str,
    binary_name: &str,
    manifest_name: &str,
) -> Result<UpdateManifest, UpdateError> {
    check_release(platform, version)?;
    let binary = fs::read(file).map_err(io_err(file))?;
    let manifest = UpdateManifest {
        platform: platform.to_owned(),
        version: version.to_owned(),
        sha256: sha256_hex(&binary),
        size: binary.len() as u64,
    };
    write_release(
        &updates_dir.join(platform),
        &[
            (binary_name, binary.as_slice()),
            (
                manifest_name,
                serde_json::to_vec_pretty(&manifest)?.as_slice(),
            ),
        ],
    )?;
    Ok(manifest)
}

/// Whether `name` is a TUI wheel's file name (and safe as a path segment).
/// Installers read the version from the name, so it is kept as built. The
/// wheel was `rmm_tui-…` up to 0.1.1; one published then is still served
/// until the next replaces it.
pub fn valid_wheel_name(name: &str) -> bool {
    name.len() <= 128
        && (name.starts_with("tetanus_rmm-") || name.starts_with("rmm_tui-"))
        && name.ends_with(".whl")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

/// Copy the TUI wheel `file` into `<updates_dir>/tui/`, replacing any wheel
/// published before. Returns its file name.
pub fn publish_tui(updates_dir: &Path, file: &Path) -> Result<String, UpdateError> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| valid_wheel_name(n))
        .ok_or_else(|| UpdateError::InvalidWheel(file.display().to_string()))?
        .to_owned();
    let bytes = fs::read(file).map_err(io_err(file))?;
    let dir = updates_dir.join(TUI_DIR);
    let previous = tui_wheels(&dir)?;
    write_release(&dir, &[(name.as_str(), bytes.as_slice())])?;
    for old in previous.iter().filter(|old| **old != name) {
        let path = dir.join(old);
        fs::remove_file(&path).map_err(io_err(&path))?;
    }
    Ok(name)
}

/// The published TUI wheel's file name, if any.
pub fn tui_wheel(updates_dir: &Path) -> Result<Option<String>, UpdateError> {
    Ok(tui_wheels(&updates_dir.join(TUI_DIR))?.into_iter().max())
}

fn tui_wheels(dir: &Path) -> Result<Vec<String>, UpdateError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err(dir)(e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry.map_err(io_err(dir))?.file_name();
        if let Some(name) = name.to_str().filter(|n| valid_wheel_name(n)) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// The published manifest for `platform`, if any.
pub fn load_manifest(
    updates_dir: &Path,
    platform: &str,
) -> Result<Option<UpdateManifest>, UpdateError> {
    load_manifest_file(updates_dir, platform, MANIFEST_FILE)
}

/// The published quick assist manifest for `platform`, if any.
pub fn load_assist_manifest(
    updates_dir: &Path,
    platform: &str,
) -> Result<Option<UpdateManifest>, UpdateError> {
    load_manifest_file(updates_dir, platform, ASSIST_MANIFEST_FILE)
}

/// The published viewer manifest for `platform`, if any.
pub fn load_viewer_manifest(
    updates_dir: &Path,
    platform: &str,
) -> Result<Option<UpdateManifest>, UpdateError> {
    load_manifest_file(updates_dir, platform, VIEWER_MANIFEST_FILE)
}

fn load_manifest_file(
    updates_dir: &Path,
    platform: &str,
    name: &str,
) -> Result<Option<UpdateManifest>, UpdateError> {
    if !valid_platform(platform) {
        return Err(UpdateError::InvalidPlatform(platform.to_owned()));
    }
    let path = updates_dir.join(platform).join(name);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(&path)(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_pair_round_trips_through_files() {
        let dir = tempfile::tempdir().unwrap();
        let key = generate_key();
        write_key_pair(dir.path(), &key).unwrap();
        let loaded = load_signing_key(&dir.path().join(SIGNING_KEY_FILE)).unwrap();
        assert_eq!(loaded.to_bytes(), key.to_bytes());
        let public = fs::read_to_string(dir.path().join(PUBLIC_KEY_FILE)).unwrap();
        assert_eq!(public, public_key_hex(&key.verifying_key()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(SIGNING_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn sign_then_publish_produces_a_verifiable_release() {
        let dir = tempfile::tempdir().unwrap();
        let key = generate_key();
        write_key_pair(dir.path(), &key).unwrap();
        let file = dir.path().join("agent-build");
        fs::write(&file, b"new agent binary").unwrap();

        let sig_path = sign_file(
            &dir.path().join(SIGNING_KEY_FILE),
            &file,
            "windows-x86_64",
            "1.2.3",
        )
        .unwrap();
        assert_eq!(sig_path, dir.path().join("agent-build.sig"));

        let updates = dir.path().join("updates");
        let manifest = publish(&updates, &file, "windows-x86_64", "1.2.3").unwrap();
        assert_eq!(
            load_manifest(&updates, "windows-x86_64").unwrap(),
            Some(manifest.clone())
        );
        assert_eq!(manifest.sha256, sha256_hex(b"new agent binary"));

        let sig: [u8; 64] = fs::read(updates.join("windows-x86_64").join(SIGNATURE_FILE))
            .unwrap()
            .try_into()
            .unwrap();
        key.verifying_key()
            .verify_strict(
                &signed_message(&manifest.platform, &manifest.version, &manifest.sha256),
                &Signature::from_bytes(&sig),
            )
            .unwrap();
    }

    #[test]
    fn rejects_bad_release_metadata() {
        let key = generate_key();
        assert!(matches!(
            sign(&key, "../x", "1.0.0", b""),
            Err(UpdateError::InvalidPlatform(_))
        ));
        assert!(matches!(
            sign(&key, "linux-x86_64", "latest", b""),
            Err(UpdateError::InvalidVersion(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_manifest(dir.path(), "a/b"),
            Err(UpdateError::InvalidPlatform(_))
        ));
        assert_eq!(load_manifest(dir.path(), "linux-x86_64").unwrap(), None);
    }

    #[test]
    fn viewer_builds_are_published_beside_agent_builds() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("viewer-build");
        fs::write(&file, b"viewer binary").unwrap();
        assert_eq!(
            load_viewer_manifest(dir.path(), "linux-x86_64").unwrap(),
            None
        );
        let manifest = publish_viewer(dir.path(), &file, "linux-x86_64", "0.3.0").unwrap();
        assert_eq!(manifest.sha256, sha256_hex(b"viewer binary"));
        assert_eq!(
            load_viewer_manifest(dir.path(), "linux-x86_64").unwrap(),
            Some(manifest)
        );
        assert_eq!(
            fs::read(dir.path().join("linux-x86_64").join(VIEWER_BINARY_FILE)).unwrap(),
            b"viewer binary"
        );
        // The agent's release is separate, and so is quick assist's.
        assert_eq!(load_manifest(dir.path(), "linux-x86_64").unwrap(), None);
        assert_eq!(
            load_assist_manifest(dir.path(), "linux-x86_64").unwrap(),
            None
        );
        let assist = publish_assist(dir.path(), &file, "linux-x86_64", "0.3.1").unwrap();
        assert_eq!(
            load_assist_manifest(dir.path(), "linux-x86_64").unwrap(),
            Some(assist)
        );
        assert_eq!(
            load_viewer_manifest(dir.path(), "linux-x86_64")
                .unwrap()
                .unwrap()
                .version,
            "0.3.0"
        );
    }

    #[test]
    fn the_tui_wheel_is_published_under_its_own_name_and_replaces_the_last() {
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path().join("updates");
        assert_eq!(tui_wheel(&updates).unwrap(), None);
        for (name, bytes) in [
            ("rmm_tui-0.1.0-py3-none-any.whl", b"one"),
            ("tetanus_rmm-0.2.0-py3-none-any.whl", b"two"),
        ] {
            let file = dir.path().join(name);
            fs::write(&file, bytes).unwrap();
            assert_eq!(publish_tui(&updates, &file).unwrap(), name);
            assert_eq!(tui_wheel(&updates).unwrap().as_deref(), Some(name));
            assert_eq!(fs::read(updates.join(TUI_DIR).join(name)).unwrap(), bytes);
        }
        assert_eq!(fs::read_dir(updates.join(TUI_DIR)).unwrap().count(), 1);

        let other = dir.path().join("something-else.whl");
        fs::write(&other, b"x").unwrap();
        assert!(matches!(
            publish_tui(&updates, &other),
            Err(UpdateError::InvalidWheel(_))
        ));
        assert!(!valid_wheel_name("tetanus_rmm-../x.whl"));
        assert!(!valid_wheel_name("tetanus_rmm-0.1.0/x.whl"));
    }

    #[test]
    fn publish_requires_a_signature() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            publish(dir.path(), &file, "linux-x86_64", "1.0.0"),
            Err(UpdateError::Io { .. })
        ));
    }
}
