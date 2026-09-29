//! Windows credential protection with DPAPI.
//!
//! `CryptProtectData` encrypts with a key derived from the calling account's
//! credentials, so the blob can only be decrypted by the same account on the
//! same machine. Installed as a service (Phase 4) that account is SYSTEM.
//! User scope is used rather than `CRYPTPROTECT_LOCAL_MACHINE`, which would
//! let any process on the machine decrypt it.

use windows::core::w;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

use super::StoreError;

/// Extra entropy so other DPAPI users under the same account cannot decrypt
/// our blob by accident.
const ENTROPY: &[u8] = b"rmm-agent-credential-v1";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    }
}

/// Copy DPAPI's output buffer and free it with `LocalFree`.
///
/// # Safety
/// `out` must have been filled in by a successful DPAPI call.
unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(Some(HLOCAL(out.pbData.cast())));
    }
    bytes
}

pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    let input = blob(plaintext);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input/entropy point at live slices for the duration of the
    // call; DPAPI only reads them. `out` is allocated by DPAPI and freed in `take`.
    unsafe {
        CryptProtectData(
            &input,
            w!("RMM agent credential"),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|e| {
            StoreError::Protect(format!(
                "{e} (DPAPI needs the account's logon credentials; a key-based SSH \
                 session does not have them. The installed service, running as \
                 SYSTEM, does.)"
            ))
        })?;
        Ok(take(out))
    }
}

pub fn unprotect(blob_bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    let input = blob(blob_bytes);
    let entropy = blob(ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as in `protect`.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .map_err(|_| StoreError::Decrypt)?;
        Ok(take(out))
    }
}
