//! Signed auto-update end to end: the server's signing and publishing tools,
//! the HTTPS update endpoints, and the agent's update client and installer.
//! No database needed: the update endpoints do not touch it.

mod support;

use std::path::Path;

use agent::update::{CheckOutcome, UpdateClient, UpdateError};
use agent::updater::{self, UpdatePaths};
use common::devcerts::DevCerts;
use ed25519_dalek::SigningKey;
use server::updates;
use sqlx::postgres::PgPoolOptions;
use support::{start_api_with, Api};

const PLATFORM: &str = "testos-x86_64";

struct Fixture {
    dir: tempfile::TempDir,
    certs: DevCerts,
    key: SigningKey,
    api: Api,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let certs = common::devcerts::generate("unused").unwrap();
        let key = updates::generate_key();
        updates::write_key_pair(&dir.path().join("keys"), &key).unwrap();
        // Lazy pool: never connects, since update routes do not use it.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .unwrap();
        let api = start_api_with(pool, &certs, dir.path().join("updates")).await;
        Self {
            dir,
            certs,
            key,
            api,
        }
    }

    fn updates_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("updates")
    }

    /// Sign with the fixture key (via the CLI code path) and publish.
    fn publish(&self, version: &str, binary: &[u8]) {
        self.publish_signed_by(&self.dir.path().join("keys/update.key"), version, binary);
    }

    fn publish_signed_by(&self, key_file: &Path, version: &str, binary: &[u8]) {
        let build = self.dir.path().join(format!("build-{version}"));
        std::fs::write(&build, binary).unwrap();
        updates::sign_file(key_file, &build, PLATFORM, version).unwrap();
        updates::publish(&self.updates_dir(), &build, PLATFORM, version).unwrap();
    }

    fn client(&self, current: &str) -> UpdateClient {
        UpdateClient::new(
            &self.api.base,
            &self.certs.ca_cert,
            self.key.verifying_key(),
            PLATFORM.into(),
            current.parse().unwrap(),
        )
        .unwrap()
    }

    fn published_file(&self, name: &str) -> std::path::PathBuf {
        self.updates_dir().join(PLATFORM).join(name)
    }
}

#[tokio::test]
async fn nothing_published_means_up_to_date() {
    let f = Fixture::new().await;
    assert!(matches!(
        f.client("0.1.0").check().await.unwrap(),
        CheckOutcome::UpToDate
    ));
}

#[tokio::test]
async fn correctly_signed_newer_release_is_downloaded_and_installed() {
    let f = Fixture::new().await;
    f.publish("0.2.0", b"agent v0.2.0");

    // Same or newer version already running: nothing to do.
    for current in ["0.2.0", "0.3.0"] {
        assert!(matches!(
            f.client(current).check().await.unwrap(),
            CheckOutcome::UpToDate
        ));
    }

    let CheckOutcome::Available(update) = f.client("0.1.0").check().await.unwrap() else {
        panic!("expected an update");
    };
    assert_eq!(update.version.to_string(), "0.2.0");
    assert_eq!(update.binary, b"agent v0.2.0");

    // Install it over a stand-in for the running executable.
    let exe = f.dir.path().join("install/agent.exe");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, b"agent v0.1.0").unwrap();
    let paths = UpdatePaths::for_exe(&exe);
    updater::stage(&paths, &update.binary).unwrap();
    updater::swap(&paths).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), b"agent v0.2.0");
    updater::cleanup(&paths);
    assert!(!paths.old.exists());
}

#[tokio::test]
async fn tampered_binary_is_rejected() {
    let f = Fixture::new().await;
    f.publish("0.2.0", b"agent v0.2.0");

    // Same length, different bytes: caught by the hash.
    std::fs::write(f.published_file(updates::BINARY_FILE), b"AGENT v0.2.0").unwrap();
    let result = f.client("0.1.0").check().await;
    assert!(
        matches!(result, Err(UpdateError::HashMismatch)),
        "{result:?}"
    );

    // Longer than the manifest says: download is cut off at the declared size.
    std::fs::write(f.published_file(updates::BINARY_FILE), b"malware v0.2.0!!").unwrap();
    let result = f.client("0.1.0").check().await;
    assert!(
        matches!(result, Err(UpdateError::TooLarge(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn tampered_binary_with_rewritten_manifest_is_rejected() {
    // An attacker who controls the update directory rewrites the manifest to
    // match their binary. Only the signature stands in the way.
    let f = Fixture::new().await;
    f.publish("0.2.0", b"agent v0.2.0");
    let evil = b"malware v0.2.0";
    std::fs::write(f.published_file(updates::BINARY_FILE), evil).unwrap();
    let manifest = protocol::update::UpdateManifest {
        platform: PLATFORM.into(),
        version: "0.2.0".into(),
        sha256: updates::sha256_hex(evil),
        size: evil.len() as u64,
    };
    std::fs::write(
        f.published_file(updates::MANIFEST_FILE),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let result = f.client("0.1.0").check().await;
    assert!(
        matches!(result, Err(UpdateError::BadSignature)),
        "{result:?}"
    );
}

#[tokio::test]
async fn release_signed_with_another_key_is_rejected() {
    let f = Fixture::new().await;
    let rogue_dir = f.dir.path().join("rogue");
    updates::write_key_pair(&rogue_dir, &updates::generate_key()).unwrap();
    f.publish_signed_by(
        &rogue_dir.join(updates::SIGNING_KEY_FILE),
        "0.2.0",
        b"agent",
    );

    let result = f.client("0.1.0").check().await;
    assert!(
        matches!(result, Err(UpdateError::BadSignature)),
        "{result:?}"
    );
}

#[tokio::test]
async fn old_signed_release_cannot_be_replayed_as_newer() {
    let f = Fixture::new().await;
    f.publish("0.1.5", b"old agent with a known bug");
    // Relabel the old, genuinely signed release as 9.0.0.
    let manifest_path = f.published_file(updates::MANIFEST_FILE);
    let mut manifest: protocol::update::UpdateManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest.version = "9.0.0".into();
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let result = f.client("0.1.0").check().await;
    assert!(
        matches!(result, Err(UpdateError::BadSignature)),
        "{result:?}"
    );
}

#[tokio::test]
async fn update_endpoints_reject_path_traversal() {
    let f = Fixture::new().await;
    for platform in ["..", "a%2F..%2Fb"] {
        let resp = f
            .api
            .client
            .get(f.api.url(&format!("/api/updates/{platform}/binary")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND, "{platform}");
    }
}
