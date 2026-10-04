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
use protocol::assist::{
    stressed, AssistConfig, WARNING_DELAY_SECS, WARNING_PAYMENT, WARNING_SUBTITLE, WARNING_TITLE,
    WARNING_UNEXPECTED,
};
use protocol::brand::Branding;
use serde::Serialize;

use super::install::{escape, house_page};
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
        branding: crate::branding::load_or_default(&state.pool).await,
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
    let branding = crate::branding::load_or_default(&state.pool).await;
    Ok(Html(render(version.as_deref(), branding.as_ref())))
}

/// A point of the scam warning, with its stressed words in bold.
fn point(icon: &brand::icons::Icon, text: &str) -> String {
    let text: String = stressed(text)
        .into_iter()
        .map(|(run, strong)| {
            if strong {
                format!("<strong>{}</strong>", escape(run))
            } else {
                escape(run)
            }
        })
        .collect();
    format!("<li>{}<span>{text}</span></li>", brand::svg::icon(icon, 22))
}

fn render(version: Option<&str>, branding: Option<&Branding>) -> String {
    // The same warning, laid out the same way, as the client's first window.
    let warning = format!(
        "<div class=\"tile\">{}</div><div><h2>{}</h2><p>{}</p></div>\
         <ul>{}{}</ul>",
        brand::svg::icon(&brand::icons::SAFETY, 28),
        escape(WARNING_TITLE),
        escape(WARNING_SUBTITLE),
        point(&brand::icons::PROHIBITED, WARNING_PAYMENT),
        point(&brand::icons::PHONE, WARNING_UNEXPECTED),
    );
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
    house_page(
        "Quick assist",
        "Let someone you trust see and control this computer, for one session.",
        WARNING_STYLE,
        &body,
        branding,
    )
}

const WARNING_STYLE: &str = r#".warning { display: grid; grid-template-columns: auto 1fr; gap: 6px 16px;
  align-items: center; background: var(--card); border: 1px solid var(--line);
  border-radius: 12px; padding: 20px 24px; margin-bottom: 16px; }
.warning .tile { width: 56px; height: 56px; border-radius: 12px; background: var(--tile);
  color: var(--link); display: flex; align-items: center; justify-content: center; }
.warning h2 { font-size: 1.3rem; margin: 0; }
.warning h2::before { content: none; }
.warning p { margin: 2px 0 0; color: var(--muted); }
.warning ul { grid-column: 1 / -1; list-style: none; margin: 10px 0 0; padding: 14px 18px;
  background: var(--panel); border-radius: 10px; display: grid; gap: 10px; }
.warning li { display: flex; gap: 12px; align-items: flex-start; }
.warning li svg { flex: none; color: var(--warn); margin-top: 2px; }"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_offers_the_download_under_the_scam_warning() {
        let page = render(Some("0.2.0"), None);
        assert!(page.contains("href=\"/assist/download\""));
        assert!(page.contains("Version 0.2.0"));
        let warning = page.find("gift cards").expect("the warning is on the page");
        assert!(warning < page.find("class=\"button\"").unwrap());
        assert!(page.contains("Do you know who's helping you?"));
        assert!(page.contains("will <strong>never</strong> ask"));
        assert!(page.contains("<b>Tetanus</b><span>RMM</span>"));
        assert!(
            !page.contains("{body}") && !page.contains("{style}") && !page.contains("{warning}")
        );
    }

    #[test]
    fn nothing_published_says_so() {
        let page = render(None, None);
        assert!(page.contains("has not been published"));
        assert!(!page.contains("class=\"button\""));
    }
}
