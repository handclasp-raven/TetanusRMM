//! Checking for, downloading and verifying signed agent updates.
//!
//! Nothing is written to disk until the signature has been verified against
//! the update public key: the one baked into this binary at build time, or
//! else the one pinned from the server (see `crate::core::update_key`).

use std::sync::Arc;

use ed25519_dalek::{Signature, VerifyingKey};
use protocol::update::{signed_message, UpdateManifest};
use ring::digest::{digest, SHA256};
use semver::Version;
use tracing::{debug, info};

/// Hex ed25519 public key baked in at build time from `RMM_UPDATE_PUBKEY`
/// (validated by build.rs). `None` means this build cannot update itself.
pub const BAKED_PUBLIC_KEY: Option<&str> = option_env!("RMM_UPDATE_PUBKEY");

/// Refuse to download anything larger than this.
pub const MAX_UPDATE_SIZE: u64 = 512 * 1024 * 1024;

/// The baked-in update key, if this build has one.
pub fn baked_public_key() -> Option<VerifyingKey> {
    BAKED_PUBLIC_KEY.map(|hex_key| parse_public_key(hex_key).expect("validated by build.rs"))
}

pub fn parse_public_key(hex_key: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(hex_key.trim()).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("tls: {0}")]
    Tls(#[from] common::TlsError),
    #[error("manifest is for platform {found:?}, this agent is {expected:?}")]
    PlatformMismatch { expected: String, found: String },
    #[error("manifest version {0:?} is not valid semver")]
    BadVersion(String),
    #[error("update is too large ({0} bytes)")]
    TooLarge(u64),
    #[error("downloaded binary does not match the manifest (size or SHA-256)")]
    HashMismatch,
    #[error("signature is malformed")]
    MalformedSignature,
    #[error("signature verification failed")]
    BadSignature,
    #[error("the server's update public key is malformed")]
    BadPublicKey,
}

/// Check a downloaded release. Succeeds only if the signature over
/// (platform, version, SHA-256 of `binary`) verifies with `key`.
pub fn verify_release(
    key: &VerifyingKey,
    expected_platform: &str,
    manifest: &UpdateManifest,
    binary: &[u8],
    signature: &[u8],
) -> Result<(), UpdateError> {
    if manifest.platform != expected_platform {
        return Err(UpdateError::PlatformMismatch {
            expected: expected_platform.to_owned(),
            found: manifest.platform.clone(),
        });
    }
    let actual_sha256 = hex::encode(digest(&SHA256, binary));
    if binary.len() as u64 != manifest.size || actual_sha256 != manifest.sha256 {
        return Err(UpdateError::HashMismatch);
    }
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| UpdateError::MalformedSignature)?;
    // The hash we computed ourselves goes into the signed message, so the
    // signature is over these exact bytes, whatever the manifest claims.
    key.verify_strict(
        &signed_message(&manifest.platform, &manifest.version, &actual_sha256),
        &Signature::from_bytes(&signature),
    )
    .map_err(|_| UpdateError::BadSignature)
}

/// An HTTPS client that trusts only `ca_pem`.
fn http_client(ca_pem: &str) -> Result<reqwest::Client, UpdateError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in common::tls::certs_from_pem(ca_pem)? {
        roots.add(cert).map_err(common::TlsError::from)?;
    }
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(common::TlsError::from)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .build()?)
}

/// The update public key the server publishes (hex), or `None` if it has
/// none. `ca_pem` is the CA the HTTPS API's certificate must chain to.
pub async fn fetch_public_key(api_url: &str, ca_pem: &str) -> Result<Option<String>, UpdateError> {
    let url = format!("{}/api/updates/pubkey", api_url.trim_end_matches('/'));
    let resp = http_client(ca_pem)?.get(url).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let text = resp.error_for_status()?.text().await?;
    let key = parse_public_key(&text).ok_or(UpdateError::BadPublicKey)?;
    Ok(Some(hex::encode(key.to_bytes())))
}

/// A downloaded, verified update, ready to install.
pub struct VerifiedUpdate {
    pub version: Version,
    pub binary: Vec<u8>,
}

impl std::fmt::Debug for VerifiedUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedUpdate")
            .field("version", &self.version)
            .field("size", &self.binary.len())
            .finish()
    }
}

#[derive(Debug)]
pub enum CheckOutcome {
    /// Nothing published, or nothing newer than this build.
    UpToDate,
    Available(VerifiedUpdate),
}

/// Polls the server's update endpoints.
pub struct UpdateClient {
    http: reqwest::Client,
    api_url: String,
    platform: String,
    current: Version,
    key: VerifyingKey,
}

impl UpdateClient {
    /// `ca_pem` is the CA the HTTPS API's certificate must chain to.
    pub fn new(
        api_url: &str,
        ca_pem: &str,
        key: VerifyingKey,
        platform: String,
        current: Version,
    ) -> Result<Self, UpdateError> {
        Ok(Self {
            http: http_client(ca_pem)?,
            api_url: api_url.trim_end_matches('/').to_owned(),
            platform,
            current,
            key,
        })
    }

    fn url(&self, leaf: &str) -> String {
        format!("{}/api/updates/{}/{leaf}", self.api_url, self.platform)
    }

    async fn get_bytes(&self, leaf: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
        let resp = self
            .http
            .get(self.url(leaf))
            .send()
            .await?
            .error_for_status()?;
        if let Some(len) = resp.content_length() {
            if len > limit {
                return Err(UpdateError::TooLarge(len));
            }
        }
        let bytes = resp.bytes().await?;
        if bytes.len() as u64 > limit {
            return Err(UpdateError::TooLarge(bytes.len() as u64));
        }
        Ok(bytes.to_vec())
    }

    /// Fetch the manifest and, if it is newer, download and verify the release.
    pub async fn check(&self) -> Result<CheckOutcome, UpdateError> {
        let resp = self.http.get(self.url("manifest")).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            debug!("no update published for this platform");
            return Ok(CheckOutcome::UpToDate);
        }
        let manifest: UpdateManifest = resp.error_for_status()?.json().await?;
        let version = Version::parse(&manifest.version)
            .map_err(|_| UpdateError::BadVersion(manifest.version.clone()))?;
        if version <= self.current {
            debug!(%version, current = %self.current, "up to date");
            return Ok(CheckOutcome::UpToDate);
        }
        if manifest.size > MAX_UPDATE_SIZE {
            return Err(UpdateError::TooLarge(manifest.size));
        }

        info!(%version, current = %self.current, "downloading update");
        let binary = self.get_bytes("binary", manifest.size).await?;
        let signature = self.get_bytes("signature", 64).await?;
        verify_release(&self.key, &self.platform, &manifest, &binary, &signature)?;
        info!(%version, "update signature verified");
        Ok(CheckOutcome::Available(VerifiedUpdate { version, binary }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const PLATFORM: &str = "windows-x86_64";

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn release(key: &SigningKey, version: &str, binary: &[u8]) -> (UpdateManifest, Vec<u8>) {
        let sha256 = hex::encode(digest(&SHA256, binary));
        let signature = key.sign(&signed_message(PLATFORM, version, &sha256));
        (
            UpdateManifest {
                platform: PLATFORM.into(),
                version: version.into(),
                sha256,
                size: binary.len() as u64,
            },
            signature.to_bytes().to_vec(),
        )
    }

    #[test]
    fn accepts_a_correctly_signed_binary() {
        let key = signing_key(1);
        let (manifest, sig) = release(&key, "1.0.0", b"good binary");
        verify_release(
            &key.verifying_key(),
            PLATFORM,
            &manifest,
            b"good binary",
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn rejects_a_tampered_binary() {
        let key = signing_key(1);
        let (manifest, sig) = release(&key, "1.0.0", b"good binary");
        let result = verify_release(
            &key.verifying_key(),
            PLATFORM,
            &manifest,
            b"evil binary",
            &sig,
        );
        assert!(
            matches!(result, Err(UpdateError::HashMismatch)),
            "{result:?}"
        );
    }

    #[test]
    fn rejects_a_tampered_binary_even_with_a_matching_manifest() {
        // An attacker who controls the server can rewrite the manifest to
        // match their binary, but cannot produce the signature.
        let key = signing_key(1);
        let (_, sig) = release(&key, "1.0.0", b"good binary");
        let evil = b"evil binary";
        let manifest = UpdateManifest {
            platform: PLATFORM.into(),
            version: "1.0.0".into(),
            sha256: hex::encode(digest(&SHA256, evil)),
            size: evil.len() as u64,
        };
        let result = verify_release(&key.verifying_key(), PLATFORM, &manifest, evil, &sig);
        assert!(
            matches!(result, Err(UpdateError::BadSignature)),
            "{result:?}"
        );
    }

    #[test]
    fn rejects_a_signature_from_another_key() {
        let (manifest, sig) = release(&signing_key(2), "1.0.0", b"bin");
        let result = verify_release(
            &signing_key(1).verifying_key(),
            PLATFORM,
            &manifest,
            b"bin",
            &sig,
        );
        assert!(matches!(result, Err(UpdateError::BadSignature)));
    }

    #[test]
    fn rejects_an_old_release_relabelled_as_new() {
        let key = signing_key(1);
        let (mut manifest, sig) = release(&key, "1.0.0", b"old binary");
        manifest.version = "9.9.9".into();
        let result = verify_release(
            &key.verifying_key(),
            PLATFORM,
            &manifest,
            b"old binary",
            &sig,
        );
        assert!(matches!(result, Err(UpdateError::BadSignature)));
    }

    #[test]
    fn rejects_other_platforms_and_malformed_signatures() {
        let key = signing_key(1);
        let (manifest, sig) = release(&key, "1.0.0", b"bin");
        assert!(matches!(
            verify_release(
                &key.verifying_key(),
                "linux-x86_64",
                &manifest,
                b"bin",
                &sig
            ),
            Err(UpdateError::PlatformMismatch { .. })
        ));
        assert!(matches!(
            verify_release(
                &key.verifying_key(),
                PLATFORM,
                &manifest,
                b"bin",
                &sig[..63]
            ),
            Err(UpdateError::MalformedSignature)
        ));
    }

    #[test]
    fn parses_public_keys() {
        let key = signing_key(3).verifying_key();
        assert_eq!(parse_public_key(&hex::encode(key.to_bytes())), Some(key));
        assert_eq!(parse_public_key("zz"), None);
        assert_eq!(parse_public_key(&"00".repeat(31)), None);
    }
}
