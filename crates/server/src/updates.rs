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

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use protocol::update::{signed_message, valid_platform, UpdateManifest};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const BINARY_FILE: &str = "agent";
pub const SIGNATURE_FILE: &str = "agent.sig";
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

    let dir = updates_dir.join(platform);
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    let manifest = UpdateManifest {
        platform: platform.to_owned(),
        version: version.to_owned(),
        sha256: sha256_hex(&binary),
        size: binary.len() as u64,
    };
    for (name, bytes) in [
        (BINARY_FILE, binary.as_slice()),
        (SIGNATURE_FILE, signature.as_slice()),
        (
            MANIFEST_FILE,
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        ),
    ] {
        let tmp = dir.join(format!(".{name}.tmp"));
        let dest = dir.join(name);
        fs::write(&tmp, bytes).map_err(io_err(&tmp))?;
        fs::rename(&tmp, &dest).map_err(io_err(&dest))?;
    }
    Ok(manifest)
}

/// The published manifest for `platform`, if any.
pub fn load_manifest(
    updates_dir: &Path,
    platform: &str,
) -> Result<Option<UpdateManifest>, UpdateError> {
    if !valid_platform(platform) {
        return Err(UpdateError::InvalidPlatform(platform.to_owned()));
    }
    let path = updates_dir.join(platform).join(MANIFEST_FILE);
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
