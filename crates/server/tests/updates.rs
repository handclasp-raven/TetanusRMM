//! Signed auto-update end to end: the server's signing and publishing tools,
//! the HTTPS update endpoints, and the agent's update client and installer.
//! No database needed: the update endpoints do not touch it.

mod support;

use std::path::Path;

use agent::core::update_key;
use agent::credstore::{Credential, CredentialStore};
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

fn credential(f: &Fixture) -> Credential {
    Credential {
        agent_id: "agt-test".into(),
        cert_pem: f.certs.agent_cert.clone(),
        key_pem: f.certs.agent_key.clone(),
        ca_pem: f.certs.ca_cert.clone(),
        server_addr: "127.0.0.1:4433".parse().unwrap(),
        server_name: "localhost".into(),
        api_url: f.api.base.clone(),
        transport: Default::default(),
        update_pubkey: None,
    }
}

// These run against agent builds without a baked-in key, as released
// builds are (RMM_UPDATE_PUBKEY must be unset when the tests are built).
#[tokio::test]
async fn an_agent_without_a_key_pins_the_published_one() {
    let f = Fixture::new().await;
    let store = CredentialStore::new(f.dir.path().join("state"));
    let enrolled = credential(&f);
    store.save(&enrolled).unwrap();

    // Nothing published yet, and nothing pinned.
    assert_eq!(
        agent::update::fetch_public_key(&f.api.base, &f.certs.ca_cert)
            .await
            .unwrap(),
        None
    );
    assert_eq!(update_key(&enrolled, Some(&store)).await.unwrap(), None);
    assert_eq!(store.load().unwrap().update_pubkey, None);

    let public = f.dir.path().join("keys").join(updates::PUBLIC_KEY_FILE);
    updates::publish_public_key(&f.updates_dir(), &public).unwrap();
    // Nowhere to pin it: not used.
    assert_eq!(update_key(&enrolled, None).await.unwrap(), None);
    let key = update_key(&enrolled, Some(&store)).await.unwrap().unwrap();
    assert_eq!(key, f.key.verifying_key());
    let pinned = store.load().unwrap();
    assert_eq!(
        pinned.update_pubkey,
        Some(updates::public_key_hex(&f.key.verifying_key()))
    );

    // The pinned key verifies a release signed by the server's key.
    f.publish("0.2.0", b"agent v0.2.0");
    let client = UpdateClient::new(
        &f.api.base,
        &f.certs.ca_cert,
        key,
        PLATFORM.into(),
        "0.1.0".parse().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        client.check().await.unwrap(),
        CheckOutcome::Available(_)
    ));
}

#[tokio::test]
async fn a_pinned_key_is_not_replaced_by_what_the_server_serves_later() {
    let f = Fixture::new().await;
    let store = CredentialStore::new(f.dir.path().join("state"));
    let public = f.dir.path().join("keys").join(updates::PUBLIC_KEY_FILE);
    updates::publish_public_key(&f.updates_dir(), &public).unwrap();
    store.save(&credential(&f)).unwrap();
    update_key(&credential(&f), Some(&store))
        .await
        .unwrap()
        .unwrap();
    let pinned = store.load().unwrap();

    // The server now serves another key.
    let other = f.dir.path().join("other-keys");
    updates::write_key_pair(&other, &updates::generate_key()).unwrap();
    updates::publish_public_key(&f.updates_dir(), &other.join(updates::PUBLIC_KEY_FILE)).unwrap();

    let key = update_key(&pinned, Some(&store)).await.unwrap().unwrap();
    assert_eq!(key, f.key.verifying_key());
    assert_eq!(store.load().unwrap(), pinned);
}

#[tokio::test]
async fn a_malformed_published_key_is_not_served_or_published() {
    let f = Fixture::new().await;
    let bad = f.dir.path().join("bad.pub");
    std::fs::write(&bad, "not a key").unwrap();
    assert!(updates::publish_public_key(&f.updates_dir(), &bad).is_err());

    std::fs::create_dir_all(f.updates_dir()).unwrap();
    std::fs::write(f.updates_dir().join(updates::PUBLIC_KEY_FILE), "not a key").unwrap();
    assert!(
        agent::update::fetch_public_key(&f.api.base, &f.certs.ca_cert)
            .await
            .is_err()
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

#[tokio::test]
async fn the_ca_and_published_viewer_builds_are_served_for_the_tui() {
    let f = Fixture::new().await;

    let resp = f.api.client.get(f.api.url("/api/ca")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), f.certs.ca_cert);

    let manifest_url = f.api.url(&format!("/api/viewer/{PLATFORM}/manifest"));
    let binary_url = f.api.url(&format!("/api/viewer/{PLATFORM}/binary"));
    for url in [&manifest_url, &binary_url] {
        assert_eq!(f.api.client.get(url).send().await.unwrap().status(), 404);
    }

    let build = f.dir.path().join("viewer-build");
    std::fs::write(&build, b"viewer v0.3.0").unwrap();
    updates::publish_viewer(&f.updates_dir(), &build, PLATFORM, "0.3.0").unwrap();

    let manifest: protocol::update::UpdateManifest = f
        .api
        .client
        .get(&manifest_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(manifest.version, "0.3.0");
    assert_eq!(manifest.sha256, updates::sha256_hex(b"viewer v0.3.0"));
    let binary = f.api.client.get(&binary_url).send().await.unwrap();
    assert_eq!(binary.bytes().await.unwrap().as_ref(), b"viewer v0.3.0");

    // A viewer build is not an agent release.
    let agent = f.api.url(&format!("/api/updates/{PLATFORM}/manifest"));
    assert_eq!(f.api.client.get(agent).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn the_install_page_offers_the_published_tui_and_viewer() {
    let f = Fixture::new().await;
    let get = |path: &str| f.api.client.get(f.api.url(path)).send();

    // The site root leads to the page (the test client follows redirects).
    let empty = get("/").await.unwrap();
    assert_eq!(empty.status(), 200);
    assert!(empty.url().path().ends_with("/install"));
    assert!(empty
        .text()
        .await
        .unwrap()
        .contains("has not been published"));

    let name = "rmm_tui-0.1.0-py3-none-any.whl";
    let wheel = f.dir.path().join(name);
    std::fs::write(&wheel, b"wheel bytes").unwrap();
    updates::publish_tui(&f.updates_dir(), &wheel).unwrap();
    let build = f.dir.path().join("viewer-build");
    std::fs::write(&build, b"viewer v0.3.0").unwrap();
    updates::publish_viewer(&f.updates_dir(), &build, "windows-x86_64", "0.3.0").unwrap();

    let page = get("/install").await.unwrap();
    assert!(page.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let page = page.text().await.unwrap();
    assert!(page.contains(&format!("href=\"/install/{name}\"")));
    assert!(page.contains("href=\"/install/viewer/windows-x86_64\""));
    // The address to type in the TUI, and the CA fingerprint it will show.
    assert!(page.contains(f.api.base.trim_start_matches("https://")));
    let fingerprint = server::quic::pem_fingerprint(&f.certs.ca_cert).unwrap();
    assert!(page.contains(&fingerprint[..47]));

    let download = get(&format!("/install/{name}")).await.unwrap();
    assert_eq!(
        download.headers()["content-disposition"],
        format!("attachment; filename=\"{name}\"")
    );
    assert_eq!(download.bytes().await.unwrap().as_ref(), b"wheel bytes");
    let viewer = get("/install/viewer/windows-x86_64").await.unwrap();
    assert_eq!(
        viewer.headers()["content-disposition"],
        "attachment; filename=\"rmm-viewer.exe\""
    );
    assert_eq!(viewer.bytes().await.unwrap().as_ref(), b"viewer v0.3.0");

    // Only the published wheel is served from there.
    for path in [
        "/install/rmm_tui-9.9.9-py3-none-any.whl",
        "/install/viewer/linux-x86_64",
        "/install/viewer/..%2Ftui",
    ] {
        assert_eq!(get(path).await.unwrap().status(), 404, "{path}");
    }
}
