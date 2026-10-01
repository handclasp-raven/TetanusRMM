//! The page a new member of staff starts from: `GET /install` (and `/`,
//! which redirects there) offers the support TUI as a download and says how
//! to install it. It needs no sign-in: it holds nothing secret, and the
//! reader has no client yet.
//!
//! The TUI is the wheel published with `server publish-tui`; the viewer is
//! fetched by the TUI itself, and is linked here only for installing by
//! hand.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{Html, Redirect, Response};
use axum::routing::get;
use axum::Router;

use super::{serve_update_file, ApiError, AppState};
use crate::updates;

/// Platforms a viewer may be published for, with the names shown.
const VIEWER_PLATFORMS: &[(&str, &str)] =
    &[("linux-x86_64", "Linux"), ("windows-x86_64", "Windows")];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/install") }))
        .route("/install", get(page))
        .route("/install/viewer/{platform}", get(viewer))
        .route("/install/{file}", get(wheel))
}

fn internal(e: updates::UpdateError) -> ApiError {
    ApiError::Internal(e.to_string())
}

fn attachment(mut resp: Response, filename: &str) -> Response {
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{filename}\"")
            .parse()
            .expect("valid header"),
    );
    resp
}

/// The published TUI wheel, under the file name pip and uv need.
async fn wheel(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let published = updates::tui_wheel(&state.updates_dir).map_err(internal)?;
    if published.as_deref() != Some(file.as_str()) {
        return Err(ApiError::NotFound);
    }
    let resp = serve_update_file(&state.updates_dir, updates::TUI_DIR, &file).await?;
    Ok(attachment(resp, &file))
}

/// The published viewer build, for installing it by hand.
async fn viewer(
    State(state): State<AppState>,
    Path(platform): Path<String>,
) -> Result<Response, ApiError> {
    let resp =
        serve_update_file(&state.updates_dir, &platform, updates::VIEWER_BINARY_FILE).await?;
    let filename = if platform.starts_with("windows") {
        "rmm-viewer.exe"
    } else {
        "rmm-viewer"
    };
    Ok(attachment(resp, filename))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn page(State(state): State<AppState>) -> Result<Html<String>, ApiError> {
    let wheel = updates::tui_wheel(&state.updates_dir).map_err(internal)?;
    let mut viewers = Vec::new();
    for (platform, name) in VIEWER_PLATFORMS {
        if let Some(manifest) =
            updates::load_viewer_manifest(&state.updates_dir, platform).map_err(internal)?
        {
            viewers.push((*platform, *name, manifest.version));
        }
    }
    // What staff type on the TUI's login screen (`https://` is assumed).
    let server = state
        .public_url
        .strip_prefix("https://")
        .unwrap_or(&state.public_url);
    let fingerprint = crate::quic::pem_fingerprint(&state.server_ca_pem).ok();
    Ok(Html(render(
        server,
        wheel.as_deref(),
        &viewers,
        fingerprint.as_deref(),
    )))
}

fn render(
    server: &str,
    wheel: Option<&str>,
    viewers: &[(&str, &str, String)],
    fingerprint: Option<&str>,
) -> String {
    let server = escape(server);
    let Some(wheel) = wheel else {
        return PAGE.replace(
            "{body}",
            "<p class=\"note\">The support TUI has not been published on this server yet. \
             Ask an administrator to run <code>scripts/build-clients.sh</code>.</p>",
        );
    };
    let wheel = escape(wheel);
    let version = wheel.split('-').nth(1).unwrap_or("").to_owned();

    let trust = match fingerprint {
        Some(fingerprint) => {
            let pairs: Vec<&str> = fingerprint.split(':').collect();
            let (first, second) = pairs.split_at(pairs.len() / 2);
            format!(
                "<p>The first time you sign in, the TUI may ask whether to trust this server \
                 and show a fingerprint. It should read:</p>\
                 <pre class=\"fingerprint\">{}\n{}</pre>\
                 <p class=\"note\">If your browser warned you about this page's certificate, \
                 confirm the fingerprint with your administrator rather than relying on this \
                 page alone.</p>",
                escape(&first.join(":")),
                escape(&second.join(":"))
            )
        }
        None => String::new(),
    };

    let viewer_links = if viewers.is_empty() {
        "<p class=\"note\">No viewer has been published on this server yet, so remote desktop \
         will not start. Ask an administrator to run <code>scripts/build-clients.sh</code>.</p>"
            .to_owned()
    } else {
        let links: Vec<String> = viewers
            .iter()
            .map(|(platform, name, version)| {
                format!(
                    "<a href=\"/install/viewer/{}\">{} ({})</a>",
                    escape(platform),
                    escape(name),
                    escape(version)
                )
            })
            .collect();
        format!(
            "<p class=\"note\">You do not need to install it yourself. To use your own copy \
             anyway, download it and set <code>viewer_path</code> in the TUI's config: {}.</p>",
            links.join(", ")
        )
    };

    let body = format!(
        r#"<ol class="steps">
<li>
<h2>Download the support TUI</h2>
<p><a class="button" href="/install/{wheel}" download>Download rmm-tui {version}</a></p>
<p class="note">One file, <code>{wheel}</code>, for Linux and Windows. Keep its name as it is.</p>
</li>
<li>
<h2>Install it</h2>
<p>The TUI is installed with <a href="https://docs.astral.sh/uv/">uv</a>, which also provides
Python. Run these in a terminal, in the folder you downloaded the file to.</p>
<h3>Windows (PowerShell)</h3>
<pre>powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"</pre>
<p class="note">Open a new PowerShell window so <code>uv</code> is found, then:</p>
<pre>cd $HOME\Downloads
uv tool install .\{wheel}</pre>
<h3>Linux</h3>
<pre>curl -LsSf https://astral.sh/uv/install.sh | sh</pre>
<p class="note">Open a new terminal so <code>uv</code> is found, then:</p>
<pre>cd ~/Downloads
uv tool install ./{wheel}</pre>
<p class="note">Already have uv or pipx? Skip the first command;
<code>pipx install ./{wheel}</code> works too. To update later, download the new file and add
<code>--force</code>.</p>
</li>
<li>
<h2>Open it and sign in</h2>
<pre>rmm-tui</pre>
<p>Enter this server, then your username, password and the code from your authenticator
app:</p>
<pre class="server">{server}</pre>
{trust}
</li>
<li>
<h2>Remote desktop</h2>
<p>The viewer is downloaded by the TUI the first time you start a remote desktop session, and
kept up to date from this server.</p>
{viewer_links}
</li>
</ol>"#
    );
    PAGE.replace("{body}", &body)
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Install the RMM support tools</title>
<style>
:root { --bg: #f6f7f9; --card: #fff; --text: #1c2330; --muted: #5d6878; --line: #dde1e7;
  --accent: #1f5fd0; --accent-text: #fff; --code: #eef1f5; }
@media (prefers-color-scheme: dark) {
  :root { --bg: #12161c; --card: #1a2029; --text: #e6e9ee; --muted: #9aa5b5; --line: #2c3442;
    --accent: #6ea2ff; --accent-text: #0d1320; --code: #10151c; }
}
* { box-sizing: border-box; }
body { margin: 0; padding: 32px 16px 64px; background: var(--bg); color: var(--text);
  font: 16px/1.55 system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 720px; margin: 0 auto; }
h1 { font-size: 1.6rem; margin: 0 0 4px; }
.lead { color: var(--muted); margin: 0 0 24px; }
.steps { list-style: none; counter-reset: step; margin: 0; padding: 0; }
.steps > li { counter-increment: step; background: var(--card); border: 1px solid var(--line);
  border-radius: 10px; padding: 20px 24px; margin-bottom: 16px; }
h2 { font-size: 1.15rem; margin: 0 0 8px; }
h2::before { content: counter(step) ". "; color: var(--muted); }
h3 { font-size: 0.95rem; margin: 18px 0 6px; }
p { margin: 8px 0; }
.note { color: var(--muted); font-size: 0.92rem; }
a { color: var(--accent); }
.button { display: inline-block; background: var(--accent); color: var(--accent-text);
  text-decoration: none; font-weight: 600; padding: 10px 18px; border-radius: 8px; }
code, pre { font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 0.88rem; }
code { background: var(--code); padding: 1px 5px; border-radius: 4px; }
pre { background: var(--code); border: 1px solid var(--line); border-radius: 8px;
  padding: 10px 12px; margin: 8px 0; overflow-x: auto; white-space: pre; }
pre.server { font-weight: 600; font-size: 1rem; }
</style>
</head>
<body>
<main>
<h1>Install the RMM support tools</h1>
<p class="lead">The support TUI and the remote desktop viewer, for Linux and Windows.</p>
{body}
</main>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_links_the_wheel_and_names_the_server() {
        let page = render(
            "rmm.example.com:8443",
            Some("rmm_tui-0.1.0-py3-none-any.whl"),
            &[("windows-x86_64", "Windows", "0.2.0".into())],
            Some("AB:CD:EF:01"),
        );
        assert!(page.contains("href=\"/install/rmm_tui-0.1.0-py3-none-any.whl\""));
        assert!(page.contains("Download rmm-tui 0.1.0"));
        assert!(page.contains("<pre class=\"server\">rmm.example.com:8443</pre>"));
        assert!(page.contains("AB:CD\nEF:01"));
        assert!(page.contains("href=\"/install/viewer/windows-x86_64\">Windows (0.2.0)"));
        assert!(!page.contains("{body}") && !page.contains("{wheel}"));
    }

    #[test]
    fn nothing_published_says_so() {
        let page = render("rmm.example.com:8443", None, &[], None);
        assert!(page.contains("has not been published"));
        assert!(!page.contains("class=\"button\""));
        let page = render("h:1", Some("rmm_tui-0.1.0-py3-none-any.whl"), &[], None);
        assert!(page.contains("No viewer has been published"));
    }

    #[test]
    fn text_is_escaped() {
        let page = render(
            "<script>",
            Some("rmm_tui-0.1.0-py3-none-any.whl"),
            &[],
            None,
        );
        assert!(page.contains("&lt;script&gt;") && !page.contains("<script>"));
    }
}
