//! Non-Windows credential protection. DEVELOPMENT ONLY.
//!
//! TODO(dev-only): this encrypts the credential with ChaCha20-Poly1305, but
//! the key sits in `credential.key` right next to it, protected only by file
//! permissions (0600). That stops casual reads and detects tampering, but
//! anyone who can read the state directory can decrypt it. It exists so the
//! agent can be developed and tested on Linux/macOS. Before shipping a
//! non-Windows agent, replace it with real OS protection (Secret Service /
//! Keychain, or a TPM-sealed key).

use std::path::Path;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

use super::{io_err, StoreError};

const KEY_FILE: &str = "credential.key";
const AAD: &[u8] = b"rmm-agent-credential-v1";

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("system RNG available");
    bytes
}

/// Load the key, creating it on first use.
fn key(dir: &Path, create: bool) -> Result<LessSafeKey, StoreError> {
    let path = dir.join(KEY_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
            let bytes = random::<32>().to_vec();
            common::fs::write_file(&path, &bytes, true).map_err(io_err(&path))?;
            bytes
        }
        Err(e) => return Err(io_err(&path)(e)),
    };
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &bytes).map_err(|_| StoreError::Decrypt)?;
    Ok(LessSafeKey::new(unbound))
}

/// `nonce || ciphertext || tag`
pub fn protect(dir: &Path, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    let key = key(dir, true)?;
    let nonce_bytes = random::<NONCE_LEN>();
    let mut sealed = plaintext.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::from(AAD),
        &mut sealed,
    )
    .map_err(|_| StoreError::Protect("encryption failed".into()))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&sealed);
    Ok(out)
}

pub fn unprotect(dir: &Path, blob: &[u8]) -> Result<Vec<u8>, StoreError> {
    if blob.len() < NONCE_LEN {
        return Err(StoreError::Decrypt);
    }
    let key = key(dir, false)?;
    let (nonce, sealed) = blob.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| StoreError::Decrypt)?;
    let mut buf = sealed.to_vec();
    let plaintext = key
        .open_in_place(nonce, Aad::from(AAD), &mut buf)
        .map_err(|_| StoreError::Decrypt)?;
    Ok(plaintext.to_vec())
}
