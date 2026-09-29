//! TOTP (RFC 6238): SHA-1, 6 digits, 30 s steps, +/-1 step of clock skew.
//! These are the parameters every common authenticator app supports.

use ring::rand::{SecureRandom, SystemRandom};
use totp_rs::{Algorithm, Builder, Secret, Totp};

/// Issuer shown in authenticator apps.
pub const ISSUER: &str = "RMM";

#[derive(Debug, thiserror::Error)]
#[error("totp: {0}")]
pub struct TotpError(String);

/// A new random 160-bit secret, base32-encoded.
pub fn generate_secret() -> String {
    let mut bytes = [0u8; 20];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG available");
    Secret::from(&bytes[..]).to_base32()
}

fn build(secret_b32: &str, account: Option<&str>) -> Result<Totp, TotpError> {
    let secret = Secret::try_from_base32(secret_b32).map_err(|e| TotpError(e.to_string()))?;
    let mut builder = Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_step_duration(30)
        .with_skew(1)
        .with_secret(secret)
        .with_issuer(Some(ISSUER));
    if let Some(account) = account {
        builder = builder.with_account_name(account);
    }
    builder.build().map_err(|e| TotpError(e.to_string()))
}

/// `otpauth://` URL for enrolling the secret in an authenticator app.
pub fn otpauth_url(secret_b32: &str, username: &str) -> Result<String, TotpError> {
    build(secret_b32, Some(username))?
        .to_url()
        .map_err(|e| TotpError(e.to_string()))
}

/// If `code` is valid at Unix time `now`, the time step it matched.
///
/// The caller must reject steps it has already accepted (see `users.totp_last_step`).
pub fn check(secret_b32: &str, code: &str, now: u64) -> Result<Option<u64>, TotpError> {
    Ok(build(secret_b32, None)?.check(code.trim(), now))
}

/// The code for Unix time `now`. For tests and tooling.
pub fn code_at(secret_b32: &str, now: u64) -> Result<String, TotpError> {
    Ok(build(secret_b32, None)?.generate(now).to_string())
}

/// Current Unix time in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B test secret ("12345678901234567890"), SHA-1.
    const RFC_SECRET: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    #[test]
    fn matches_rfc6238_vectors() {
        // RFC values are 8 digits; the 6-digit code is the last 6.
        assert_eq!(code_at(RFC_SECRET, 59).unwrap(), "287082");
        assert_eq!(code_at(RFC_SECRET, 1_111_111_109).unwrap(), "081804");
        assert_eq!(code_at(RFC_SECRET, 2_000_000_000).unwrap(), "279037");
    }

    #[test]
    fn accepts_current_and_adjacent_steps_only() {
        let now = 1_790_000_000;
        let step = now / 30;
        let code = code_at(RFC_SECRET, now).unwrap();
        assert_eq!(check(RFC_SECRET, &code, now).unwrap(), Some(step));
        assert_eq!(check(RFC_SECRET, &code, now + 30).unwrap(), Some(step));
        assert_eq!(check(RFC_SECRET, &code, now - 30).unwrap(), Some(step));
        assert_eq!(check(RFC_SECRET, &code, now + 90).unwrap(), None);
    }

    #[test]
    fn rejects_malformed_codes() {
        let now = 1_790_000_000;
        for bad in ["", "12345", "1234567", "abcdef"] {
            assert_eq!(check(RFC_SECRET, bad, now).unwrap(), None, "{bad:?}");
        }
    }

    #[test]
    fn generated_secrets_are_unique_and_usable() {
        let a = generate_secret();
        assert_ne!(a, generate_secret());
        let code = code_at(&a, 1_000).unwrap();
        assert!(check(&a, &code, 1_000).unwrap().is_some());
    }

    #[test]
    fn otpauth_url_names_issuer_and_account() {
        let url = otpauth_url(RFC_SECRET, "alice").unwrap();
        assert!(url.starts_with("otpauth://totp/RMM:alice?"), "{url}");
        assert!(url.contains(&format!("secret={RFC_SECRET}")), "{url}");
    }
}
