//! Argon2id password hashing.
//!
//! Uses the argon2 crate defaults: Argon2id v19, m=19 MiB, t=2, p=1 (the
//! OWASP minimum recommendation). Parameters are stored in the PHC string,
//! so they can be raised later without invalidating existing hashes.

use std::sync::OnceLock;

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;

#[derive(Debug, thiserror::Error)]
#[error("password hashing failed: {0}")]
pub struct HashError(String);

/// Hash `password` into a `$argon2id$...` PHC string.
///
/// Runs on the blocking pool: hashing deliberately takes tens of milliseconds.
pub async fn hash(password: String) -> Result<String, HashError> {
    tokio::task::spawn_blocking(move || hash_blocking(&password))
        .await
        .map_err(|e| HashError(e.to_string()))?
}

fn hash_blocking(password: &str) -> Result<String, HashError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| HashError(e.to_string()))
}

/// Check `password` against `stored`.
///
/// With `stored = None` (unknown user), still performs a full verification
/// against a dummy hash, so response time does not reveal whether the
/// username exists. Always returns false in that case.
pub async fn verify(password: String, stored: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        let known = stored.is_some();
        let hash = stored.unwrap_or_else(|| dummy_hash().to_owned());
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), hash.as_str())
            .is_ok();
        ok && known
    })
    .await
    .unwrap_or(false)
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_blocking("dummy password for timing").expect("hashing a constant"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_with_argon2id_and_verifies() {
        let h = hash("correct horse battery".into()).await.unwrap();
        assert!(h.starts_with("$argon2id$v=19$"), "{h}");
        assert!(verify("correct horse battery".into(), Some(h.clone())).await);
        assert!(!verify("wrong horse battery".into(), Some(h)).await);
    }

    #[tokio::test]
    async fn same_password_gets_a_different_salt() {
        let a = hash("same password here".into()).await.unwrap();
        let b = hash("same password here".into()).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn unknown_user_never_verifies() {
        assert!(!verify("dummy password for timing".into(), None).await);
    }

    #[tokio::test]
    async fn malformed_stored_hash_does_not_verify() {
        assert!(!verify("anything".into(), Some("not a phc string".into())).await);
    }
}
