//! The server's HTTPS API, for the panels: the agent's status, the
//! command buttons and file transfer.
//!
//! The viewer's own connection (see [`crate::client`]) only carries the
//! session. These calls act as the signed-in technician, with the session
//! token the TUI hands over in the environment (`RMM_API_TOKEN`), so the
//! server checks their access and audits them like any other client.
//! Without it the viewer still works; the panel just has nothing to show.

use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use ring::digest::{Context, SHA256};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Header carrying a file's hex SHA-256 (see the server's files API).
const SHA256_HEADER: &str = "x-content-sha256";

/// Bytes read from disk at a time, when hashing and uploading.
const READ_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    /// HTTP status, if the server answered.
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    fn local(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    /// An upload refused because the file is already there.
    pub fn already_exists(&self) -> bool {
        self.status == Some(409) && self.message.contains("already exists")
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        let mut message = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(inner) = source {
            message = format!("{message}: {inner}");
            source = inner.source();
        }
        Self {
            status: e.status().map(|s| s.as_u16()),
            message,
        }
    }
}

/// One disk of the agent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Disk {
    pub name: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
}

/// What the Status tab shows, from `GET /api/agents/{id}`. Everything the
/// agent reports is optional: an older agent, or one that has not reported
/// yet, leaves it out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AgentDetails {
    pub id: String,
    pub hostname: Option<String>,
    #[serde(default)]
    pub online: bool,
    pub os: Option<String>,
    pub local_ip: Option<String>,
    pub remote_ip: Option<String>,
    pub dns_servers: Option<Vec<String>>,
    pub logged_in_users: Option<Vec<String>>,
    pub disks: Option<Vec<Disk>>,
    /// What the technician may do here (`desktop`, `file_transfer`, ...).
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl AgentDetails {
    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// Whether the agent runs Windows (paths like `C:\...`). Assumed until
    /// it says otherwise: that is what agents run in production.
    pub fn is_windows(&self) -> bool {
        self.os
            .as_deref()
            .is_none_or(|os| os.starts_with("Windows"))
    }
}

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl ApiClient {
    /// A client for the API at `base` (`https://host:8443`) that trusts
    /// only `ca_pem`, as the viewer's own connection does.
    pub fn new(base: &str, token: String, ca_pem: &str) -> anyhow::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in common::tls::certs_from_pem(ca_pem)? {
            roots.add(cert)?;
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn files_url(&self, agent_id: &str, remote: &str, overwrite: bool) -> Result<String, ApiError> {
        let mut url = reqwest::Url::parse(&self.url(&format!("/api/agents/{agent_id}/files")))
            .map_err(|e| ApiError::local(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("path", remote)
            .append_pair("overwrite", &overwrite.to_string());
        Ok(url.into())
    }

    /// The response, or the server's `{"error": ...}` as an [`ApiError`].
    async fn check(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let message = body["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| match status.as_u16() {
                401 => "the TUI session has expired: sign in again".into(),
                403 => "not allowed".into(),
                _ => status.to_string(),
            });
        Err(ApiError {
            status: Some(status.as_u16()),
            message,
        })
    }

    pub async fn agent(&self, agent_id: &str) -> Result<AgentDetails, ApiError> {
        let response = self
            .http
            .get(self.url(&format!("/api/agents/{agent_id}")))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        Ok(Self::check(response).await?.json().await?)
    }

    /// Start `command` on the agent's desktop. Returns who it runs as.
    pub async fn launch(&self, agent_id: &str, command: &str) -> Result<String, ApiError> {
        let response = self
            .http
            .post(self.url(&format!("/api/agents/{agent_id}/launch")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "command": command }))
            .timeout(std::time::Duration::from_secs(45))
            .send()
            .await?;
        let body: serde_json::Value = Self::check(response).await?.json().await?;
        Ok(body["user"].as_str().unwrap_or_default().to_owned())
    }

    /// Upload `local` to `remote` on the agent. `progress(sent, total)` is
    /// called as it goes. Returns the size.
    pub async fn upload(
        &self,
        agent_id: &str,
        local: &Path,
        remote: &str,
        overwrite: bool,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> Result<u64, ApiError> {
        let (size, sha256) = hash_file(local).await?;
        let file = tokio::fs::File::open(local)
            .await
            .map_err(|e| ApiError::local(format!("cannot open {}: {e}", local.display())))?;
        let progress = Arc::new(progress);
        let body = futures_util::stream::unfold((file, 0u64), move |(mut file, sent)| {
            let progress = progress.clone();
            async move {
                let mut buffer = vec![0; READ_CHUNK];
                match file.read(&mut buffer).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buffer.truncate(n);
                        let sent = sent + n as u64;
                        progress(sent, size);
                        Some((Ok::<_, std::io::Error>(buffer), (file, sent)))
                    }
                    Err(e) => Some((Err(e), (file, sent))),
                }
            }
        });
        let response = self
            .http
            .put(self.files_url(agent_id, remote, overwrite)?)
            .bearer_auth(&self.token)
            .header(SHA256_HEADER, hex::encode(sha256))
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(reqwest::Body::wrap_stream(body))
            .send()
            .await?;
        Self::check(response).await?;
        Ok(size)
    }

    /// Download `remote` from the agent to `local`, checked against the
    /// agent's SHA-256. Written beside it first, so a failed download
    /// leaves nothing behind. Returns the size.
    pub async fn download(
        &self,
        agent_id: &str,
        remote: &str,
        local: &Path,
        progress: impl Fn(u64, u64),
    ) -> Result<u64, ApiError> {
        let response = self
            .http
            .get(self.files_url(agent_id, remote, false)?)
            .bearer_auth(&self.token)
            .send()
            .await?;
        let response = Self::check(response).await?;
        let expected = response
            .headers()
            .get(SHA256_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| ApiError::local("the server sent no checksum"))?;
        let total = response.content_length().unwrap_or(0);
        let partial = partial_path(local);
        let result = async {
            let mut file = tokio::fs::File::create(&partial)
                .await
                .map_err(|e| ApiError::local(format!("cannot write {}: {e}", partial.display())))?;
            let mut hash = Context::new(&SHA256);
            let mut received = 0u64;
            let mut body = response.bytes_stream();
            while let Some(chunk) = body.next().await {
                let chunk = chunk?;
                hash.update(&chunk);
                file.write_all(&chunk)
                    .await
                    .map_err(|e| ApiError::local(format!("writing {}: {e}", partial.display())))?;
                received += chunk.len() as u64;
                progress(received, total);
            }
            file.flush()
                .await
                .map_err(|e| ApiError::local(e.to_string()))?;
            if hex::encode(hash.finish()) != expected.trim().to_ascii_lowercase() {
                return Err(ApiError::local(
                    "the download is corrupt (checksum mismatch)",
                ));
            }
            tokio::fs::rename(&partial, local)
                .await
                .map_err(|e| ApiError::local(format!("cannot write {}: {e}", local.display())))?;
            Ok(received)
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
        }
        result
    }
}

/// Size and SHA-256 of a local file.
async fn hash_file(path: &Path) -> Result<(u64, [u8; 32]), ApiError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ApiError::local(format!("cannot open {}: {e}", path.display())))?;
    let mut hash = Context::new(&SHA256);
    let mut buffer = vec![0; READ_CHUNK];
    let mut size = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .await
            .map_err(|e| ApiError::local(format!("reading {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    let digest = hash.finish();
    Ok((
        size,
        digest.as_ref().try_into().expect("SHA-256 is 32 bytes"),
    ))
}

fn partial_path(local: &Path) -> std::path::PathBuf {
    let mut name = local.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    local.with_file_name(name)
}

/// The last component of a path on the agent (`C:\a\b.txt` -> `b.txt`).
pub fn remote_file_name(remote: &str) -> &str {
    remote
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_details_parse_with_or_without_status() {
        let full: AgentDetails = serde_json::from_value(serde_json::json!({
            "id": "agt-1",
            "hostname": "WS-01",
            "online": true,
            "os": "Windows 11 Pro (build 26100)",
            "local_ip": "192.168.1.20",
            "remote_ip": "203.0.113.9",
            "dns_servers": ["192.168.1.1"],
            "logged_in_users": ["CORP\\alice"],
            "disks": [{ "name": "C:\\", "total_bytes": 100, "used_bytes": 40 }],
            "capabilities": ["desktop", "file_transfer"],
            "cpu_percent": 12.5,
        }))
        .unwrap();
        assert_eq!(full.disks.as_ref().unwrap()[0].used_bytes, 40);
        assert!(full.can("file_transfer") && !full.can("shell"));
        assert!(full.is_windows());
        let bare: AgentDetails =
            serde_json::from_value(serde_json::json!({ "id": "agt-2" })).unwrap();
        assert_eq!(bare.disks, None);
        assert!(!bare.online && bare.is_windows());
        let linux = AgentDetails {
            os: Some("Linux (Ubuntu 24.04)".into()),
            ..bare
        };
        assert!(!linux.is_windows());
    }

    #[test]
    fn remote_file_names() {
        assert_eq!(remote_file_name(r"C:\Users\alice\report.pdf"), "report.pdf");
        assert_eq!(remote_file_name("/var/log/syslog"), "syslog");
        assert_eq!(remote_file_name(r"C:\Temp\"), "Temp");
        assert_eq!(remote_file_name(""), "");
    }

    #[test]
    fn partial_downloads_sit_beside_the_target() {
        assert_eq!(
            partial_path(Path::new("/home/me/report.pdf")),
            Path::new("/home/me/report.pdf.part")
        );
    }

    #[test]
    fn upload_conflicts_are_recognised() {
        let exists = ApiError {
            status: Some(409),
            message: "the file already exists (set overwrite to replace it)".into(),
        };
        assert!(exists.already_exists());
        let offline = ApiError {
            status: Some(409),
            message: "agent is not connected".into(),
        };
        assert!(!offline.already_exists());
    }
}
