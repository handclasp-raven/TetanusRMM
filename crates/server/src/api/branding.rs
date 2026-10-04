//! Company branding (see `crate::branding`): anyone may read it (the
//! install pages and installers need it before anyone has signed in, and
//! it holds nothing secret); admins set and reset it. A change goes to
//! every connected agent at once.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use protocol::brand::{base64, Branding};
use protocol::Message;
use serde::{Deserialize, Serialize};

use super::{rbac, ApiError, AppState, Session};
use crate::branding;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/branding", get(show).put(update).delete(reset))
}

/// The branding as the API speaks it.
#[derive(Debug, Serialize, Deserialize)]
struct BrandingBody {
    name: String,
    /// `#RRGGBB`, or null for TetanusRMM's rust.
    #[serde(default)]
    accent: Option<String>,
    /// A PNG, base64; null for TetanusRMM's mark.
    #[serde(default)]
    logo_png: Option<String>,
}

impl From<Branding> for BrandingBody {
    fn from(b: Branding) -> Self {
        Self {
            accent: b.accent_rgb().map(brand::Rgb::to_hex),
            logo_png: b.logo_png.as_deref().map(base64::encode),
            name: b.name,
        }
    }
}

fn internal(e: sqlx::Error) -> ApiError {
    ApiError::Internal(e.to_string())
}

/// The branding in force; `null` when it is TetanusRMM's own.
async fn show(State(state): State<AppState>) -> Result<Json<Option<BrandingBody>>, ApiError> {
    let current = branding::load(&state.pool).await.map_err(internal)?;
    Ok(Json(current.map(BrandingBody::from)))
}

/// Tell every connected agent that can show it.
fn announce(state: &AppState, branding: Option<Branding>) {
    if let Some(hub) = &state.hub {
        hub.broadcast(&Message::Branding(branding), protocol::MIN_BRANDING_VERSION);
    }
}

async fn update(
    State(state): State<AppState>,
    session: Session,
    Json(body): Json<BrandingBody>,
) -> Result<Json<BrandingBody>, ApiError> {
    rbac::require_admin(&state.pool, &session.user, "branding.update").await?;
    let bad = |message: String| ApiError::BadRequest(message);
    let accent = match body.accent.as_deref() {
        None => None,
        Some(text) => Some(
            brand::Rgb::parse(text)
                .ok_or_else(|| bad("the accent colour must be #RRGGBB".into()))?
                .to_array(),
        ),
    };
    let logo_png = body
        .logo_png
        .as_deref()
        .map(base64::decode)
        .transpose()
        .map_err(|_| bad("the logo must be base64".into()))?;
    let new = Branding {
        name: body.name.trim().to_owned(),
        accent,
        logo_png,
    };
    new.validate().map_err(|e| bad(e.to_string()))?;
    branding::save(&state.pool, &session.user.username, &new)
        .await
        .map_err(internal)?;
    announce(&state, Some(new.clone()));
    Ok(Json(new.into()))
}

async fn reset(State(state): State<AppState>, session: Session) -> Result<(), ApiError> {
    rbac::require_admin(&state.pool, &session.user, "branding.update").await?;
    branding::clear(&state.pool, &session.user.username)
        .await
        .map_err(internal)?;
    announce(&state, None);
    Ok(())
}
