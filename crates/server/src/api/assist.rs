//! Quick assist (see `crate::assist`): codes for technicians, and the page
//! a user is sent to.
//!
//! `GET /assist` needs no sign-in: whoever opens it has no account, and it
//! holds nothing secret. It offers the quick assist client for Windows.
//! The client is the same build for every server; `GET /assist/download`
//! appends this server's address and CA certificate to it
//! (`protocol::assist`), so the user has nothing to configure.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use protocol::assist::{AssistConfig, SCAM_WARNING, WARNING_DELAY_SECS};
use serde::Serialize;

use super::install::{escape, PAGE_STYLE};
use super::{install_target, ApiError, AppState, Session};
use crate::assist::{self, AssistError};
use crate::updates;

/// The platform the quick assist client is built for.
const PLATFORM: &str = super::MSI_PLATFORM;

/// What the downloaded file is called.
const FILE_NAME: &str = "TetanusRMM-Assist.exe";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/assist-sessions", post(create))
        .route("/api/assist-sessions/{id}", get(status))
        .route("/assist", get(page))
        .route("/assist/download", get(download))
}

#[derive(Serialize)]
struct NewCodeResponse {
    id: i64,
    /// Six digits, to read out to the user. Shown once.
    code: String,
    expires_at: DateTime<Utc>,
    /// The page to send the user to.
    url: String,
}

/// Make a quick assist code.
async fn create(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<NewCodeResponse>, ApiError> {
    if !session.user.role.can_view_desktop() {
        return Err(ApiError::Forbidden);
    }
    let code = assist::create(&state.pool, &session.user)
        .await
        .map_err(|e| match e {
            AssistError::Db(e) => e.into(),
            other => ApiError::Internal(other.to_string()),
        })?;
    Ok(Json(NewCodeResponse {
        id: code.id,
        code: code.code,
        expires_at: code.expires_at,
        url: format!("{}/assist", state.public_url),
    }))
}

/// Where a quick assist session is: whether the code has been typed, and
/// the agent to start a viewer for once it has.
async fn status(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<i64>,
) -> Result<Json<assist::Status>, ApiError> {
    assist::status(&state.pool, state.hub.as_deref(), id, &session.user)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// The quick assist client with this server's settings appended.
async fn download(State(state): State<AppState>) -> Result<Response, ApiError> {
    let path = state
        .updates_dir
        .join(PLATFORM)
        .join(updates::ASSIST_BINARY_FILE);
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError::NotFound),
        Err(e) => return Err(ApiError::Internal(format!("{}: {e}", path.display()))),
    };
    let len = file
        .metadata()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .len();
    let target = install_target(&state.public_url, None, None)?;
    let trailer = protocol::assist::trailer(&AssistConfig {
        server: target.server,
        server_name: target.server_name,
        ca_pem: state.server_ca_pem.clone(),
    });
    let total = len + trailer.len() as u64;
    let body =
        tokio_util::io::ReaderStream::new(file).chain(futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(axum::body::Bytes::from(trailer))
        }));
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, total.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{FILE_NAME}\""),
            ),
        ],
        Body::from_stream(body),
    )
        .into_response())
}

async fn page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let version = updates::load_assist_manifest(&state.updates_dir, PLATFORM)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map(|manifest| manifest.version);
    Ok(Html(render(version.as_deref())))
}

fn render(version: Option<&str>) -> String {
    let warning: String = SCAM_WARNING
        .split("\n\n")
        .map(|paragraph| format!("<p>{}</p>", escape(paragraph)))
        .collect();
    let body = match version {
        None => "<p class=\"note\">Quick assist has not been published on this server yet. \
                 Ask the person supporting you to let their administrator know.</p>"
            .to_owned(),
        Some(version) => format!(
            r#"<div class="warning">{warning}</div>
<ol class="steps">
<li>
<h2>Download</h2>
<p><a class="button" href="/assist/download" download>Download quick assist</a></p>
<p class="note">For Windows. Version {version}. Nothing is installed: it is one file that you
can delete afterwards.</p>
</li>
<li>
<h2>Open it</h2>
<p>Open <code>{FILE_NAME}</code> from your downloads. Windows may ask whether to allow it to
make changes; choosing Yes lets your supporter help with more, and No still works.</p>
<p>You are shown a warning about scams and must wait {WARNING_DELAY_SECS} seconds before you
can continue. Please read it.</p>
</li>
<li>
<h2>Type the code</h2>
<p>Type the six-digit code the person supporting you gives you. You are then asked, by their
name, whether to let them see and control your screen.</p>
<p class="note">Close the window, or press Ctrl+F12, to end the session at any time.</p>
</li>
</ol>"#,
            version = escape(version),
        ),
    };
    PAGE.replace("{style}", PAGE_STYLE).replace("{body}", &body)
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Quick assist</title>
<style>
{style}
.warning { border: 2px solid var(--warn); border-radius: 10px; padding: 12px 24px;
  margin-bottom: 16px; background: var(--card); font-weight: 600; }
</style>
</head>
<body>
<main>
<h1>Quick assist</h1>
<p class="lead">Let someone you trust see and control this computer, for one session.</p>
{body}
</main>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_offers_the_download_under_the_scam_warning() {
        let page = render(Some("0.2.0"));
        assert!(page.contains("href=\"/assist/download\""));
        assert!(page.contains("Version 0.2.0"));
        let warning = page.find("gift cards").expect("the warning is on the page");
        assert!(warning < page.find("class=\"button\"").unwrap());
        assert!(
            !page.contains("{body}") && !page.contains("{style}") && !page.contains("{warning}")
        );
    }

    #[test]
    fn nothing_published_says_so() {
        let page = render(None);
        assert!(page.contains("has not been published"));
        assert!(!page.contains("class=\"button\""));
    }
}
