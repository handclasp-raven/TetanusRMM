//! A secret kept encrypted in this process's memory: the password a user
//! lends the technicians (see `protocol::credential`).
//!
//! `CryptProtectMemory` encrypts in place with a key the kernel keeps for
//! this process, so the bytes in our address space, and so in a crash dump
//! or the page file, are ciphertext. The buffer is locked in RAM as well,
//! and wiped when dropped. The plaintext exists only for the length of
//! [`ProtectedSecret::with_plain`].
//!
//! This does not stop someone who can run code as this process (for the
//! service, SYSTEM): they can ask for the same decryption.

use protocol::ipc::Secret;
use windows::Win32::Security::Cryptography::{
    CryptProtectMemory, CryptUnprotectMemory, CRYPTPROTECTMEMORY_BLOCK_SIZE,
    CRYPTPROTECTMEMORY_SAME_PROCESS,
};
use windows::Win32::System::Memory::{VirtualLock, VirtualUnlock};
use zeroize::Zeroize;

/// UTF-16 units per block `CryptProtectMemory` works in.
const BLOCK_UNITS: usize = CRYPTPROTECTMEMORY_BLOCK_SIZE as usize / 2;

pub struct ProtectedSecret {
    /// Ciphertext, a whole number of blocks. Never reallocated.
    buf: Vec<u16>,
    /// Units of plaintext before the padding.
    len: usize,
}

impl ProtectedSecret {
    pub fn new(secret: &Secret) -> windows::core::Result<Self> {
        let units = secret.units();
        let padded = units.len().div_ceil(BLOCK_UNITS).max(1) * BLOCK_UNITS;
        let mut this = Self {
            buf: vec![0; padded],
            len: units.len(),
        };
        // Best effort: it fails only if the working set's quota is used up.
        // SAFETY: the range is the buffer we own.
        let _ = unsafe { VirtualLock(this.buf.as_ptr().cast(), this.bytes()) };
        this.buf[..units.len()].copy_from_slice(units);
        // If this fails, dropping `this` wipes the plaintext.
        this.protect()?;
        Ok(this)
    }

    fn bytes(&self) -> usize {
        self.buf.len() * 2
    }

    fn protect(&mut self) -> windows::core::Result<()> {
        // SAFETY: the buffer is ours and a multiple of the block size.
        unsafe {
            CryptProtectMemory(
                self.buf.as_mut_ptr().cast(),
                self.bytes() as u32,
                CRYPTPROTECTMEMORY_SAME_PROCESS,
            )
        }
    }

    /// Decrypt in place, hand the plaintext to `f`, and encrypt again. On
    /// an error the secret is no longer usable: drop it.
    pub fn with_plain<R>(&mut self, f: impl FnOnce(&[u16]) -> R) -> windows::core::Result<R> {
        // SAFETY: as in `protect`.
        unsafe {
            CryptUnprotectMemory(
                self.buf.as_mut_ptr().cast(),
                self.bytes() as u32,
                CRYPTPROTECTMEMORY_SAME_PROCESS,
            )?;
        }
        let result = f(&self.buf[..self.len]);
        if let Err(e) = self.protect() {
            self.buf.zeroize();
            return Err(e);
        }
        Ok(result)
    }
}

impl Drop for ProtectedSecret {
    fn drop(&mut self) {
        self.buf.zeroize();
        // SAFETY: the range we locked in `new`.
        let _ = unsafe { VirtualUnlock(self.buf.as_ptr().cast(), self.bytes()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_holds_ciphertext_and_gives_the_secret_back() {
        let plain: Vec<u16> = "correct horse battery".encode_utf16().collect();
        let mut secret = ProtectedSecret::new(&Secret::new(plain.clone())).unwrap();
        assert_eq!(secret.buf.len() % BLOCK_UNITS, 0);
        assert_ne!(secret.buf[..plain.len()], plain[..]);
        for _ in 0..2 {
            assert_eq!(secret.with_plain(<[u16]>::to_vec).unwrap(), plain);
            assert_ne!(secret.buf[..plain.len()], plain[..]);
        }
    }
}
