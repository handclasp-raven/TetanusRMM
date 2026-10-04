//! The server's company branding (see `protocol::brand`): what an admin
//! set, kept in the database, for the agents, quick assist, MSIs and the
//! install pages. At most one; none means TetanusRMM's own look.

use protocol::brand::Branding;
use serde_json::json;
use sqlx::PgPool;

use crate::audit::{self, Action, NewEntry};

/// The table's row: the name, the accent's three bytes, the logo.
type Row = (String, Option<Vec<u8>>, Option<Vec<u8>>);

/// The branding in force, if an admin has set one.
pub async fn load(pool: &PgPool) -> sqlx::Result<Option<Branding>> {
    let row: Option<Row> = sqlx::query_as("SELECT name, accent, logo_png FROM branding")
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(name, accent, logo_png)| Branding {
        name,
        accent: accent.and_then(|a| <[u8; 3]>::try_from(a).ok()),
        logo_png,
    }))
}

/// [`load`], with a database error taken as none: the look falls back to
/// TetanusRMM's rather than a page or a download failing.
pub async fn load_or_default(pool: &PgPool) -> Option<Branding> {
    match load(pool).await {
        Ok(branding) => branding,
        Err(e) => {
            tracing::warn!("reading the branding: {e}");
            None
        }
    }
}

/// Set the branding (already validated). Audited as `branding.update`.
pub async fn save(pool: &PgPool, actor: &str, branding: &Branding) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO branding (name, accent, logo_png, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (only_row) DO UPDATE SET name = EXCLUDED.name, accent = EXCLUDED.accent,
             logo_png = EXCLUDED.logo_png, updated_by = EXCLUDED.updated_by, updated_at = now()",
    )
    .bind(&branding.name)
    .bind(branding.accent.map(|a| a.to_vec()))
    .bind(&branding.logo_png)
    .bind(actor)
    .execute(pool)
    .await?;
    audit::append_now(
        pool,
        NewEntry::new(actor, Action::BrandingUpdate).detail(json!({
            "name": branding.name,
            "accent": branding.accent_rgb().map(brand::Rgb::to_hex),
            "logo": branding.logo_png.is_some(),
        })),
    )
    .await
    .map(drop)
}

/// Back to TetanusRMM's own look. Whether there was anything to remove;
/// audited as `branding.update` if so.
pub async fn clear(pool: &PgPool, actor: &str) -> sqlx::Result<bool> {
    let removed = sqlx::query("DELETE FROM branding")
        .execute(pool)
        .await?
        .rows_affected()
        > 0;
    if removed {
        audit::append_now(
            pool,
            NewEntry::new(actor, Action::BrandingUpdate).detail(json!({ "reset": true })),
        )
        .await?;
    }
    Ok(removed)
}
